// phantom-core owns the domain model and its SQLite persistence.
// Nothing in this crate knows about HTTP, MCP, or the CLI.
//
// The scan domain (scan/scanner/treemap/format + ScanStore) is the product.

pub mod backup;
pub mod classify;
pub mod diff;
pub mod format;
pub mod persist;
pub mod scan;
pub mod scanner;
pub mod schema;
pub mod store;
pub mod treemap;
pub mod wire_time;

pub use classify::{Category, Classification, HotspotGroup, HotspotRule, HotspotsSummary};
pub use diff::{DiffEntry, ScanDiff};
pub use format::FileTypeTotal;
pub use persist::{PERSIST_MIN_FILE_DISK_SIZE, persistable_entries};
pub use scan::{Scan, ScanEntry, ScanRequest, ScanStatus, TreemapLayout, TreemapRect, UnreadablePath};
pub use scanner::{ProgressSnapshot, ScanError, ScanOutcome, ScanProgress};
pub use store::{FilePage, FileQuery, FileSort, ScanStore};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("schema error: {0}")]
    Schema(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
