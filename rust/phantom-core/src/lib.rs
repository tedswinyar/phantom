// phantom-core owns the domain model and its SQLite persistence.
// Nothing in this crate knows about HTTP, MCP, or the CLI.
//
// The scan domain (scan/scanner/treemap/format + ScanStore) is the product.

pub mod backup;
pub mod bulk;
pub mod classify;
pub mod diff;
pub mod discovery;
pub mod format;
pub mod gitignore;
pub mod growth;
pub mod insight;
pub mod persist;
pub mod plan;
pub mod probe;
pub mod scan;
pub mod scanner;
pub mod schema;
pub mod share;
pub mod store;
pub mod treemap;
pub mod volume;
pub mod wire_time;

pub use classify::{
    Category, ClassifyOptions, Classification, HotspotGroup, HotspotRule, HotspotsSummary, LockVerdict,
    ProjectActivity, ProjectArtifact, RebuildCost, RebuildKind, RiskTier, ToolEstimate, parse_older_than,
};
pub use insight::{PathExplanation, PathHotspot, StaleProject, StaleProjects};
pub use volume::{HiddenSpace, UserHome, VolumeStatus};
pub use growth::{GroupBy, Growth, GrowthForecast, GrowthLine, GrowthPoint};
pub use diff::{DiffEntry, ScanDiff, SinceSpec};
pub use format::{ChargeKey, FileTypeTotal, LinkCharger};
pub use persist::{PERSIST_MIN_FILE_DISK_SIZE, persistable_entries};
pub use plan::{PlanOptions, PlanSkipped, ReclaimPlan, ReclaimPlanItem, ReclaimVerification, VerifyItem};
pub use scan::{
    EntryFlags, Scan, ScanEntry, ScanRequest, ScanStatus, TreemapLayout, TreemapRect, UnreadablePath,
};
pub use scanner::{ProgressSnapshot, ScanError, ScanOptions, ScanOutcome, ScanProgress};
pub use share::ShareLedger;
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
