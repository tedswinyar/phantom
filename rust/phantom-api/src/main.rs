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

    let state = AppState::new(scan_store, api_key);

    // Port ladder: try the configured port, then climb. Whatever binds is
    // REPORTED ON STDOUT as the first line — clients and supervisors must
    // parse this rather than assume the configured port.
    let listener = bind_with_ladder(config.port).await;
    let addr = listener.local_addr().expect("listener has a local addr");
    println!("phantom-api listening on http://127.0.0.1:{}", addr.port());
    use std::io::Write;
    std::io::stdout().flush().ok();

    // The drain also cancels in-flight scans: their walker threads see the
    // flag at the next entry and hand off a `cancelled` row before the
    // runtime shuts down.
    let registry = state.registry.clone();
    let shutdown = async move {
        shutdown_signal().await;
        let cancelled = registry.cancel_all();
        if cancelled > 0 {
            tracing::info!(scans = cancelled, "cancelling in-flight scans for shutdown");
        }
    };

    if let Err(e) = axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!("server error: {e}");
        eprintln!("phantom-api: server error: {e}");
        std::process::exit(1);
    }
    tracing::info!("shutdown complete");
}

/// Resolve once SIGINT (Ctrl-C) or SIGTERM arrives, so axum can stop
/// accepting new connections and drain in-flight requests before the process
/// exits. Supervisors (including the Swift app's `APIServerManager`) send
/// SIGTERM; without this the process is killed mid-request.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to install Ctrl-C handler: {e}");
            // Never resolve: fall back to SIGTERM-only shutdown.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("SIGINT received; draining"),
        () = terminate => tracing::info!("SIGTERM received; draining"),
    }
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
