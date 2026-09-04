// phantom-api — the hub. Every client (CLI, MCP, Swift app) talks HTTP
// to this server; nothing else opens the database.
//
// Concurrency ceiling, stated honestly: the store is a single SQLite
// connection behind one `std::sync::Mutex`, so EVERY request — reads
// included — is fully serialized (it is a Mutex, not an RwLock). For a
// single-user local tool this is correct and cheap: each handler locks,
// does sub-millisecond synchronous SQLite work, and drops the guard without
// awaiting, so the "std Mutex held across .await" deadlock footgun is
// avoided. It does NOT scale to concurrent clients or heavy/slow queries —
// the moment either arrives, move to a connection pool (r2d2/deadpool-sqlite
// with WAL) or wrap DB calls in `spawn_blocking`. This is a deliberate
// ceiling for the target use case, not "no lock contention".

pub mod auth;
pub mod config;
pub mod registry;
pub mod routes;
pub mod scans;

use std::sync::{Arc, Mutex, MutexGuard};

use phantom_core::ScanStore;

use registry::ScanRegistry;

#[derive(Clone)]
pub struct AppState {
    pub scan_store: Arc<Mutex<ScanStore>>,
    /// In-flight scans: live progress + cancel flags (see `registry`).
    pub registry: Arc<ScanRegistry>,
    pub api_key: String,
    /// Test-only pacing point: when set, every scan worker waits for one
    /// `notify_one()` before starting its walk, so integration tests can
    /// order cancel/list/delete against a deterministically-running scan.
    /// Always `None` in production (`AppState::new` sets it).
    pub scan_hold: Option<Arc<tokio::sync::Notify>>,
}

impl AppState {
    pub fn new(scan_store: ScanStore, api_key: String) -> Self {
        Self {
            scan_store: Arc::new(Mutex::new(scan_store)),
            registry: Arc::new(ScanRegistry::new()),
            api_key,
            scan_hold: None,
        }
    }

    /// Poison-tolerant access to the store. A panic in one handler while
    /// holding the lock poisons the mutex; recovering the inner guard here
    /// (rather than `.unwrap()`-panicking) keeps a single bad request from
    /// bricking every subsequent one. The store's operations are ACID per
    /// statement, so a recovered guard still sees a consistent database.
    pub fn scan_store(&self) -> MutexGuard<'_, ScanStore> {
        self.scan_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn build_router(state: AppState) -> axum::Router {
    routes::router(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutation-proof (Testing standard): revert `scan_store()` to
    // `.lock().unwrap()` and this test panics instead of passing.
    #[test]
    fn scan_store_access_survives_a_poisoned_mutex() {
        let state = AppState::new(ScanStore::open_in_memory().unwrap(), "k".into());
        let poisoner = Arc::clone(&state.scan_store);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("simulated handler panic while holding the scan store lock");
        })
        .join();
        assert!(state.scan_store.is_poisoned());

        assert!(state.scan_store().list_scans().unwrap().is_empty());
    }
}
