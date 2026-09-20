use phantom_api::{AppState, auth, build_router, config::Config};
use phantom_core::ScanStore;

/// How far up the port ladder to climb when the configured port is busy.
const PORT_LADDER_STEPS: u16 = 10;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("phantom-api: config error: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(?config.profile, db = %config.db_path.display(), "starting");

    let scan_store = match ScanStore::open(&config.db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "phantom-api: cannot open database {}: {e}",
                config.db_path.display()
            );
            std::process::exit(1);
        }
    };
    let api_key = match auth::load_or_create_key(&config.key_file) {
        Ok(k) => k,
        Err(e) => {
            eprintln!(
                "phantom-api: cannot read/create key file {}: {e}",
                config.key_file.display()
            );
            std::process::exit(1);
        }
    };

    let state = AppState::with_retention(scan_store, api_key, config.retention);

    // Port ladder: try the configured port, then climb. Whatever binds is
    // REPORTED ON STDOUT as the first line — clients and supervisors must
    // parse this rather than assume the configured port.
    // Install the signal handlers BEFORE binding and announcing: tokio
    // registers them lazily on first poll, and a supervisor that sends
    // SIGTERM right after reading the announcement would otherwise hit the
    // default action — instant death, no drain, no unpublish (found by
    // test_discovery.rs, 2026-09-09).
    let signals = ShutdownSignals::install();
    let listener = bind_with_ladder(config.port).await;
    let addr = listener.local_addr().expect("listener has a local addr");
    let announced = format!("http://127.0.0.1:{}", addr.port());
    // Publish the bound URL for clients that cannot hear stdout (the CLI,
    // the MCP server): written BEFORE the announcement so a supervisor that
    // has read the line can rely on the file (phantom_core::discovery).
    if let Err(e) = phantom_core::discovery::publish_url(&config.url_file, &announced) {
        tracing::warn!(file = %config.url_file.display(), "could not publish the API url: {e}");
    }
    println!("phantom-api listening on {announced}");
    use std::io::Write;
    std::io::stdout().flush().ok();

    // The drain also cancels in-flight scans: their walker threads see the
    // flag at the next entry and hand off a `cancelled` row before the
    // runtime shuts down. It fires on SIGINT/SIGTERM, or when the supervising
    // app named by PHANTOM_SUPERVISOR_PID is gone (phantom-85r).
    let registry = state.registry.clone();
    let supervisor = supervisor_pid_from_env();
    let shutdown = async move {
        tokio::select! {
            () = signals.wait() => {}
            () = supervisor_gone(supervisor) => {
                tracing::warn!("supervising process exited; shutting down");
            }
        }
        let cancelled = registry.cancel_all();
        if cancelled > 0 {
            tracing::info!(scans = cancelled, "cancelling in-flight scans for shutdown");
        }
    };

    let served = axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown)
        .await;
    // Only OUR url is removed: a newer server may already have published.
    match phantom_core::discovery::unpublish_url(&config.url_file, &announced) {
        Ok(true) => tracing::info!("unpublished the API url"),
        Ok(false) => {}
        Err(e) => tracing::warn!("could not unpublish the API url: {e}"),
    }
    if let Err(e) = served {
        tracing::error!("server error: {e}");
        eprintln!("phantom-api: server error: {e}");
        std::process::exit(1);
    }
    tracing::info!("shutdown complete");
}

/// SIGINT (Ctrl-C) and SIGTERM, installed EAGERLY at construction so a
/// signal that arrives before the server is polled still takes the graceful
/// path: axum stops accepting, drains in-flight requests, cancels scans and
/// unpublishes the URL. Supervisors (including the Swift app's
/// `APIServerManager`) send SIGTERM; without this the process would be
/// killed mid-request.
struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl ShutdownSignals {
    fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let interrupt = signal(SignalKind::interrupt())
                .map_err(|e| tracing::warn!("failed to install SIGINT handler: {e}"))
                .ok();
            let terminate = signal(SignalKind::terminate())
                .map_err(|e| tracing::warn!("failed to install SIGTERM handler: {e}"))
                .ok();
            Self { interrupt, terminate }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    async fn wait(mut self) {
        #[cfg(unix)]
        {
            let interrupt = async {
                match self.interrupt.as_mut() {
                    Some(sig) => {
                        sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            let terminate = async {
                match self.terminate.as_mut() {
                    Some(sig) => {
                        sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                () = interrupt => tracing::info!("SIGINT received; draining"),
                () = terminate => tracing::info!("SIGTERM received; draining"),
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await
        }
    }
}

/// `PHANTOM_SUPERVISOR_PID`: the pid of the process that owns this server
/// (the Swift app sets it to its own). Unset or unparsable == no supervisor,
/// the server lives until signalled — the right answer for `start.sh` and
/// for anyone running the binary by hand.
fn supervisor_pid_from_env() -> Option<i32> {
    let raw = std::env::var("PHANTOM_SUPERVISOR_PID").ok()?;
    match raw.trim().parse::<i32>() {
        Ok(pid) if pid > 0 => Some(pid),
        _ => {
            tracing::warn!(value = %raw, "ignoring unparsable PHANTOM_SUPERVISOR_PID");
            None
        }
    }
}

/// How often the supervisor is checked. A dead app leaks a headless server
/// for at most this long.
const SUPERVISOR_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Resolve once the supervising process no longer exists. `kill(pid, 0)`
/// sends nothing; ESRCH means the pid is gone (EPERM means it exists but is
/// not ours — still alive, keep waiting). Never resolves without a
/// supervisor. (phantom-85r: ten crashed dev launches once left ten orphans
/// holding the whole port ladder.)
async fn supervisor_gone(pid: Option<i32>) {
    let Some(pid) = pid else {
        std::future::pending::<()>().await;
        unreachable!()
    };
    tracing::info!(pid, "watching supervising process");
    loop {
        tokio::time::sleep(SUPERVISOR_POLL).await;
        if !process_exists(pid) {
            return;
        }
    }
}

fn process_exists(pid: i32) -> bool {
    // SAFETY: kill with signal 0 performs only the existence/permission
    // check; no signal is delivered.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

async fn bind_with_ladder(base_port: u16) -> tokio::net::TcpListener {
    // Port 0 = ephemeral, no ladder needed.
    let steps = if base_port == 0 { 1 } else { PORT_LADDER_STEPS };
    for offset in 0..steps {
        let port = base_port.saturating_add(offset);
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return l,
            Err(e) => {
                tracing::warn!(port, "bind failed: {e}");
            }
        }
    }
    eprintln!(
        "phantom-api: no free port in {base_port}..{}",
        base_port.saturating_add(PORT_LADDER_STEPS)
    );
    std::process::exit(1);
}
