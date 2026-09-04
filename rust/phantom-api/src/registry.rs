// The in-flight side of the scan lifecycle. A running scan lives here —
// identity, live progress counters, and its cooperative-cancel flag — until
// it reaches a terminal state and is handed off to SQLite (see
// `scans::finish_scan` for the ordering that makes the handoff seamless).
//
// The registry holds only bookkeeping; the walk's entries stay on the worker
// thread's stack until persistence. Terminal scans normally leave the
// registry the moment their row is in the database — the only long-lived
// resident with a terminal status is a scan whose DB insert failed, kept
// visible as `failed` rather than vanishing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::Utc;
use phantom_core::{ProgressSnapshot, Scan, ScanProgress, ScanStatus};
use uuid::Uuid;

struct ScanHandle {
    scan: Scan,
    progress: Arc<ScanProgress>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct ScanRegistry {
    inner: Mutex<HashMap<Uuid, ScanHandle>>,
}

impl ScanRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Poison-tolerant lock, same rationale as `AppState::store`.
    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, ScanHandle>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Track a freshly started scan; returns the progress counters and
    /// cancel flag the walker thread shares with observers.
    pub fn register(&self, scan: Scan) -> (Arc<ScanProgress>, Arc<AtomicBool>) {
        let progress = Arc::new(ScanProgress::new());
        let cancel = Arc::new(AtomicBool::new(false));
        self.lock().insert(
            scan.id,
            ScanHandle {
                scan,
                progress: Arc::clone(&progress),
                cancel: Arc::clone(&cancel),
            },
        );
        (progress, cancel)
    }

    /// Point-in-time view of one in-flight scan.
    pub fn snapshot(&self, id: Uuid) -> Option<(Scan, ProgressSnapshot)> {
        self.lock()
            .get(&id)
            .map(|h| (h.scan.clone(), h.progress.snapshot()))
    }

    /// Point-in-time view of every in-flight scan (unordered; callers sort).
    pub fn list(&self) -> Vec<(Scan, ProgressSnapshot)> {
        self.lock()
            .values()
            .map(|h| (h.scan.clone(), h.progress.snapshot()))
            .collect()
    }

    /// Flip a scan's cancel flag. Returns false when the scan is not (or no
    /// longer) in flight. A true return means the request was accepted, not
    /// that the walk has already stopped — callers poll for the terminal
    /// status.
    pub fn request_cancel(&self, id: Uuid) -> bool {
        match self.lock().get(&id) {
            Some(h) => {
                h.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Whether cancellation was requested for an in-flight scan.
    pub fn cancel_requested(&self, id: Uuid) -> bool {
        self.lock()
            .get(&id)
            .is_some_and(|h| h.cancel.load(Ordering::Relaxed))
    }

    pub fn remove(&self, id: Uuid) {
        self.lock().remove(&id);
    }

    /// Last resort when the terminal handoff to SQLite fails: keep the scan
    /// visible as failed instead of letting it vanish.
    pub fn mark_failed(&self, id: Uuid) {
        if let Some(h) = self.lock().get_mut(&id) {
            h.scan.status = ScanStatus::Failed;
            h.scan.finished_at = Some(Utc::now());
        }
    }

    /// SIGTERM drain: flip every in-flight scan's cancel flag so the walker
    /// threads bail at their next entry. Returns how many were signalled.
    pub fn cancel_all(&self) -> usize {
        let guard = self.lock();
        for h in guard.values() {
            h.cancel.store(true, Ordering::Relaxed);
        }
        guard.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(root: &str) -> Scan {
        Scan::new(root)
    }

    #[test]
    fn register_snapshot_list_remove_round_trip() {
        let r = ScanRegistry::new();
        let scan = running("/tmp/a");
        let id = scan.id;
        let (progress, _cancel) = r.register(scan.clone());

        progress.snapshot(); // counters exist and start at zero
        let (got, snap) = r.snapshot(id).unwrap();
        assert_eq!(got, scan);
        assert_eq!((snap.files_seen, snap.bytes_seen), (0, 0));
        assert_eq!(r.list().len(), 1);

        r.remove(id);
        assert!(r.snapshot(id).is_none());
        assert!(r.list().is_empty());
    }

    // Mutation target: make request_cancel forget the `store(true)` and the
    // shared-flag assertion fails; make it return true for unknown ids and
    // the second assertion fails.
    #[test]
    fn request_cancel_flips_the_shared_flag_only_for_known_scans() {
        let r = ScanRegistry::new();
        let scan = running("/tmp/a");
        let id = scan.id;
        let (_, cancel) = r.register(scan);

        assert!(!r.cancel_requested(id));
        assert!(r.request_cancel(id));
        assert!(cancel.load(Ordering::Relaxed), "walker's flag must flip");
        assert!(r.cancel_requested(id));

        assert!(!r.request_cancel(Uuid::new_v4()), "unknown id is refused");
    }

    #[test]
    fn cancel_all_signals_every_in_flight_scan() {
        let r = ScanRegistry::new();
        let (_, c1) = r.register(running("/tmp/a"));
        let (_, c2) = r.register(running("/tmp/b"));
        assert_eq!(r.cancel_all(), 2);
        assert!(c1.load(Ordering::Relaxed));
        assert!(c2.load(Ordering::Relaxed));
    }

    #[test]
    fn mark_failed_sets_a_terminal_visible_state() {
        let r = ScanRegistry::new();
        let scan = running("/tmp/a");
        let id = scan.id;
        r.register(scan);
        r.mark_failed(id);
        let (got, _) = r.snapshot(id).unwrap();
        assert_eq!(got.status, ScanStatus::Failed);
        assert!(got.finished_at.is_some());
    }
}
