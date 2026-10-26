// The scan domain's wire models. Everything here crosses an implementation
// boundary, so it follows the wire-format contract (camelCase keys at every
// depth, nullable-present-as-null, wire_time datetimes, lowercase UUIDs out).
//
// `diskSize` (st_blocks × 512) is THE size everywhere; `logicalSize` is a
// secondary field kept for cloud-dataloaded detection (logical ≫ disk).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CoreError, Result};

/// Lifecycle of a scan. Stored in SQLite as the lowercase wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanStatus {
    Running,
    Complete,
    Cancelled,
    Failed,
}

impl ScanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScanStatus::Running => "running",
            ScanStatus::Complete => "complete",
            ScanStatus::Cancelled => "cancelled",
            ScanStatus::Failed => "failed",
        }
    }
}

impl std::str::FromStr for ScanStatus {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(ScanStatus::Running),
            "complete" => Ok(ScanStatus::Complete),
            "cancelled" => Ok(ScanStatus::Cancelled),
            "failed" => Ok(ScanStatus::Failed),
            other => Err(CoreError::InvalidInput(format!(
                "unknown scan status: {other:?}"
            ))),
        }
    }
}

/// One entry the walker counted in `error_count`, with the OS's reason —
/// "Mail is protected" and "the disk is dying" must be distinguishable
/// (phantom-671; the count alone cannot tell an FDA gap from hardware).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadablePath {
    pub path: String,
    /// The OS error text, e.g. "Operation not permitted (os error 1)".
    pub reason: String,
}

/// One scan of a directory tree: metadata and totals, no entries embedded
/// (entries are big and live in their own table / their own endpoints).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scan {
    pub id: Uuid,
    pub root_path: String,
    pub status: ScanStatus,
    #[serde(with = "crate::wire_time")]
    pub started_at: DateTime<Utc>,
    /// Set when the scan reaches a terminal status. Nullable on the wire,
    /// never absent.
    #[serde(with = "crate::wire_time::option")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Sum of file diskSize (st_blocks × 512) — THE headline number.
    pub total_disk_size: u64,
    /// Sum of file logical size; secondary, kept for dataloaded detection.
    pub total_logical_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    /// Entries skipped because they could not be read (permissions, races).
    pub error_count: u64,
    /// A capped SAMPLE (first [`crate::scanner::UNREADABLE_SAMPLE_CAP`], walk
    /// order) of the entries behind `error_count` — the count stays the
    /// truth. Filled when a scan completes; empty until then and for
    /// cancelled/failed scans (partial results are discarded). `None` ==
    /// not recorded: rows persisted before schema v4 (same null-vs-empty
    /// contract as entry counts).
    pub unreadable_paths: Option<Vec<UnreadablePath>>,
    /// What deleting the whole tree would free: the root directory's
    /// `privateSize` (see [`ScanEntry::private_size`]). ≤ `totalDiskSize`.
    /// `None` == not recorded: rows persisted before schema v5.
    pub total_private_size: Option<u64>,
    /// Bytes under the root that are also referenced from outside it, or
    /// trapped in clones/snapshots (see [`ScanEntry::shared_size`]).
    /// `None` == not recorded (pre-v5).
    pub total_shared_size: Option<u64>,
    /// Why a `failed` scan failed — the walker's error, or the cold-start
    /// marker for a scan the server was running when it stopped
    /// ("interrupted: …"). Null for every other status. Present-as-null on
    /// the wire; absent in pre-1.1 bodies, which decode as null.
    #[serde(default)]
    pub failure_reason: Option<String>,
}

impl Scan {
    /// A freshly started scan: running, no totals yet.
    pub fn new(root_path: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            root_path: root_path.into(),
            status: ScanStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            total_disk_size: 0,
            total_logical_size: 0,
            file_count: 0,
            dir_count: 0,
            error_count: 0,
            unreadable_paths: Some(Vec::new()),
            total_private_size: Some(0),
            total_shared_size: Some(0),
            failure_reason: None,
        }
    }
}

/// Per-entry facts the walker reads from the filesystem that are not sizes:
/// APFS clone/sparse/purgeable bits, dataless (cloud placeholder), firmlink,
/// mount point. On the wire a SORTED array of camelCase strings (`["dataless",
/// "purgeable"]`); in SQLite a bitmask. Decoders IGNORE unknown strings so a
/// newer server can add a flag without breaking an older client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct EntryFlags(u32);

impl EntryFlags {
    /// `EF_MAY_SHARE_BLOCKS`: some of this file's blocks may be shared with
    /// another file (it is, or was cloned from, a clone).
    pub const MAY_SHARE_BLOCKS: EntryFlags = EntryFlags(1 << 0);
    /// `EF_SHARES_ALL_BLOCKS`: a pure clone — every block is shared, so
    /// deleting it frees nothing (`privateSize == 0`).
    pub const SHARES_ALL_BLOCKS: EntryFlags = EntryFlags(1 << 1);
    /// `EF_IS_PURGEABLE`: the OS may reclaim it under pressure.
    pub const PURGEABLE: EntryFlags = EntryFlags(1 << 2);
    /// `EF_IS_SPARSE`: has at least one hole (logical ≫ disk).
    pub const SPARSE: EntryFlags = EntryFlags(1 << 3);
    /// `SF_DATALESS`: a cloud placeholder whose contents are not local.
    pub const DATALESS: EntryFlags = EntryFlags(1 << 4);
    /// `SF_FIRMLINK`: a system-volume firmlink into the data volume
    /// (`/Users`, `/Applications`, …). Followed by the walker — it is the
    /// OS's own unified view, one physical copy.
    pub const FIRMLINK: EntryFlags = EntryFlags(1 << 5);
    /// `DIR_MNTSTATUS_MNTPOINT`: another filesystem is mounted here. NOT
    /// descended unless the scan asked to cross volumes; the row is kept
    /// so the boundary is visible.
    pub const MOUNT_POINT: EntryFlags = EntryFlags(1 << 6);
    /// `UF_COMPRESSED`: transparently compressed (decmpfs) — logical ≫ disk
    /// WITHOUT being a cloud placeholder, and its clone attributes are
    /// meaningless (the data is in the resource fork), so `privateSize` is
    /// the allocation.
    pub const COMPRESSED: EntryFlags = EntryFlags(1 << 7);

    /// Wire names, in bit order. The wire array is emitted in THIS order.
    const NAMES: [(EntryFlags, &'static str); 8] = [
        (Self::MAY_SHARE_BLOCKS, "mayShareBlocks"),
        (Self::SHARES_ALL_BLOCKS, "sharesAllBlocks"),
        (Self::PURGEABLE, "purgeable"),
        (Self::SPARSE, "sparse"),
        (Self::DATALESS, "dataless"),
        (Self::FIRMLINK, "firmlink"),
        (Self::MOUNT_POINT, "mountPoint"),
        (Self::COMPRESSED, "compressed"),
    ];

    pub const fn empty() -> Self {
        EntryFlags(0)
    }
    pub fn bits(self) -> u32 {
        self.0
    }
    /// From the stored bitmask; unknown bits are dropped, never invented.
    pub fn from_bits(bits: u32) -> Self {
        let known = Self::NAMES.iter().fold(0, |acc, (f, _)| acc | f.0);
        EntryFlags(bits & known)
    }
    pub fn contains(self, other: EntryFlags) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn insert(&mut self, other: EntryFlags) {
        self.0 |= other.0;
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(f, _)| self.contains(*f))
            .map(|(_, n)| *n)
            .collect()
    }
    fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = EntryFlags::empty();
        for name in names {
            if let Some((f, _)) = Self::NAMES.iter().find(|(_, n)| *n == name) {
                out.insert(*f);
            }
        }
        out
    }
}

impl std::ops::BitOr for EntryFlags {
    type Output = EntryFlags;
    fn bitor(self, rhs: EntryFlags) -> EntryFlags {
        EntryFlags(self.0 | rhs.0)
    }
}

impl Serialize for EntryFlags {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.names().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EntryFlags {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let names: Vec<String> = Vec::deserialize(deserializer)?;
        Ok(EntryFlags::from_names(names.iter().map(String::as_str)))
    }
}

/// One filesystem entry within a scan. `scan_id` is contextual (every read
/// path is scoped to a scan), so it lives in the table, not the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanEntry {
    pub path: String,
    /// `None` exactly for the scan root; every other entry's parent is
    /// inside the scan.
    pub parent_path: Option<String>,
    pub name: String,
    pub is_dir: bool,
    /// st_blocks × 512. Directories carry 0; aggregate via
    /// `format::directory_disk_totals` or the treemap.
    pub disk_size: u64,
    pub logical_size: u64,
    #[serde(with = "crate::wire_time::option")]
    pub modified_at: Option<DateTime<Utc>>,
    /// Lowercased extension; `None` for directories and extensionless files.
    pub file_type: Option<String>,
    /// Reclaimability category. NULL until Phase 5's classifier fills it.
    pub category: Option<String>,
    /// Hardlink metadata for Phase-5 dedup: entries sharing (dev, ino) with
    /// nlink > 1 are the same physical file and must be counted once.
    pub nlink: u64,
    pub dev: u64,
    pub ino: u64,
    /// Directory rows: descendant FILES at full depth, counted from the FULL
    /// walk — sub-1-MiB files count here even though their rows are never
    /// persisted (a client-side count is a structural undercount). File rows:
    /// null. Also null on directory rows persisted before schema v3 (the
    /// walk is gone; a backfill from persisted rows would be the same lie).
    pub file_count: Option<u64>,
    /// Same contract for descendant DIRECTORIES at full depth, excluding the
    /// entry itself.
    pub dir_count: Option<u64>,
    /// What deleting this path would ACTUALLY free, in bytes. Files: 0 when
    /// other hard links keep the inode alive (`nlink > 1`); otherwise the
    /// blocks not shared with any other file or snapshot (APFS
    /// `ATTR_CMNEXT_PRIVATESIZE` — a pure clone reports 0, a modified clone
    /// only its rewritten blocks); `diskSize` when the filesystem cannot
    /// say. Directory rows: what deleting the whole subtree frees — every
    /// hardlink/clone group whose EVERY reference lies inside the subtree
    /// counts once at full allocation; a group with references outside it
    /// contributes nothing. `None` == not recorded: rows persisted before
    /// schema v5.
    pub private_size: Option<u64>,
    /// Bytes this entry references that deleting it would NOT free — held
    /// by other hard links, other clones, or snapshots. Files:
    /// `diskSize − privateSize`. Directories: allocation of every group
    /// that has a reference inside the subtree and another outside it,
    /// plus the shared portion of partially-cloned files. `None` == not
    /// recorded (pre-v5).
    pub shared_size: Option<u64>,
    /// APFS clone-group id (`ATTR_CMNEXT_CLONEID`) when this file shares
    /// ALL its blocks with at least one other file — every member of the
    /// group carries the same id and the group's bytes count ONCE per scan,
    /// exactly like a hardlinked inode. `None` == not a pure clone (or
    /// the filesystem cannot say, or pre-v5 row). Directories: always null.
    pub clone_id: Option<u64>,
    /// Filesystem facts about the entry (see [`EntryFlags`]); `[]` when
    /// there are none. `None` == not recorded (pre-v5).
    pub flags: Option<EntryFlags>,
}

/// Inbound request to start a scan. `deny_unknown_fields` because a client
/// sending a field we ignore should find out, not scan the wrong thing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScanRequest {
    pub root_path: String,
    /// Descend into directories that are mount points for other volumes
    /// (`/Volumes/*`, `/System/Volumes/Data`, network mounts). Default
    /// false — `du -x` semantics: one filesystem, so scanning `/` never
    /// counts the data volume twice through both its mount point and the
    /// system-volume firmlinks into it. Firmlinks are followed either way.
    #[serde(default)]
    pub cross_volumes: bool,
    /// Staleness threshold for the classifier: a project whose newest
    /// source edit and git activity are at least this old is dormant.
    /// `90d`, `12w`, `3M`, `1y`, or bare days; default 90 days
    /// (`classify::DORMANT_AFTER_DAYS`). Validated at request time (400).
    #[serde(default)]
    pub older_than: Option<String>,
    /// Opt-in (v1.1): run each regenerable project's read-only lockfile
    /// check (`cargo metadata --locked`, `npm ci --dry-run`, `uv lock
    /// --locked`) from fixed absolute paths, inside the project directory.
    /// A failure lowers the tier; nothing is written. See
    /// docs/threat-model.md §4 before enabling on untrusted checkouts.
    #[serde(default)]
    pub verify_locks: bool,
    /// Opt-in (v1.1): attach the owning tool's own dry-run number
    /// (`docker system df`, `brew cleanup -n`, `uv cache size`) to the
    /// matching hotspot groups as `toolEstimate`. Fixed absolute paths only.
    #[serde(default)]
    pub tool_estimates: bool,
}

/// A rectangle in the treemap layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreemapRect {
    pub path: String,
    pub name: String,
    /// diskSize (aggregated for directories).
    pub size: u64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub depth: usize,
    pub is_dir: bool,
    pub file_type: Option<String>,
    /// True for a synthesized "smaller files" pseudo-tile: the visible
    /// remainder of a directory whose persisted children under-sum its
    /// aggregate (the ADR-0005 sub-1-MiB folding made honest). Its `path`
    /// is the PARENT directory's path — a hit resolves to the parent.
    /// Always present on the wire; false for every real rect.
    pub residual: bool,
}

/// Treemap layout for a directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreemapLayout {
    pub root_path: String,
    pub total_size: u64,
    pub rects: Vec<TreemapRect>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // Parser tests work from raw bytes, not from round-tripping our own
    // encoder — round-tripping can't catch casing or format drift. The
    // fixtures are shared with the Swift tests and the OPE conformance
    // harness (tests/fixtures/README.md) so the wire format cannot drift.
    //
    // The scan fixtures carry the API's wire VIEW — `Scan` fields plus a
    // `progress` key (object while running, null once terminal). `Scan`
    // itself ignores `progress` on decode; `wire_view_progress_shape_is_
    // pinned_by_the_fixtures` below pins that extra key's shape.
    const RAW_SCAN_COMPLETE: &str = include_str!("../../../tests/fixtures/scan-complete.json");
    const RAW_SCAN_RUNNING: &str = include_str!("../../../tests/fixtures/scan-running.json");
    const RAW_SCAN_INTERRUPTED: &str =
        include_str!("../../../tests/fixtures/scan-interrupted.json");
    const RAW_ENTRY_FILE: &str = include_str!("../../../tests/fixtures/entry.json");
    const RAW_ENTRY_ROOT_DIR: &str = include_str!("../../../tests/fixtures/entry-dir.json");
    const RAW_TREEMAP: &str = include_str!("../../../tests/fixtures/treemap.json");

    #[test]
    fn decodes_complete_scan_from_raw_bytes() {
        let s: Scan = serde_json::from_str(RAW_SCAN_COMPLETE).unwrap();
        assert_eq!(s.root_path, "/Users/ghost/Code");
        assert_eq!(s.status, ScanStatus::Complete);
        assert_eq!(s.total_disk_size, 123_456_789);
        assert_eq!(s.total_logical_size, 223_456_789);
        assert_eq!(s.file_count, 4200);
        assert_eq!(s.dir_count, 310);
        assert_eq!(s.error_count, 2);
        assert!(s.finished_at.is_some());
        // v1.1 totals: what deleting the tree frees vs what is pinned.
        assert_eq!(s.total_private_size, Some(100_000_000));
        assert_eq!(s.failure_reason, None, "a complete scan has no failure reason");
        assert_eq!(s.total_shared_size, Some(23_456_789));
        // The unreadable sample coheres with errorCount and carries reasons.
        let sample = s.unreadable_paths.as_ref().unwrap();
        assert_eq!(
            sample,
            &vec![
                UnreadablePath {
                    path: "/Users/ghost/Code/locked".into(),
                    reason: "Permission denied (os error 13)".into(),
                },
                UnreadablePath {
                    path: "/Users/ghost/Code/vanished.tmp".into(),
                    reason: "No such file or directory (os error 2)".into(),
                },
            ]
        );
    }

    #[test]
    fn decodes_running_scan_with_null_finished_at() {
        let s: Scan = serde_json::from_str(RAW_SCAN_RUNNING).unwrap();
        assert_eq!(s.status, ScanStatus::Running);
        assert_eq!(s.finished_at, None);
        // Running scans have recorded nothing yet — empty, not null.
        assert_eq!(s.unreadable_paths, Some(Vec::new()));
        assert_eq!((s.total_private_size, s.total_shared_size), (Some(0), Some(0)));
    }

    // Pre-v5 rows: the share totals are null ("not recorded") and must
    // decode as None; encode keeps the keys present-as-null (wire rule 3).
    #[test]
    fn null_share_totals_round_trip_as_not_recorded() {
        let mut v: serde_json::Value = serde_json::from_str(RAW_SCAN_COMPLETE).unwrap();
        v["totalPrivateSize"] = serde_json::Value::Null;
        v["totalSharedSize"] = serde_json::Value::Null;
        let s: Scan = serde_json::from_value(v).unwrap();
        assert_eq!((s.total_private_size, s.total_shared_size), (None, None));
        let out = serde_json::to_value(&s).unwrap();
        for key in ["totalPrivateSize", "totalSharedSize"] {
            assert!(out.as_object().unwrap().contains_key(key), "{key} must be present");
            assert!(out[key].is_null());
        }
    }

    // Pre-v4 rows: unreadablePaths is null ("not recorded", distinct from
    // "recorded, none failed") and must decode as None; encode keeps the
    // key present-as-null (wire rule 3).
    /// The cold-start marker (phantom-aoa): a scan the server was running
    /// when it stopped is persisted as `failed` with a reason that says so.
    #[test]
    fn interrupted_fixture_is_failed_with_a_reason() {
        let s: Scan = serde_json::from_str(RAW_SCAN_INTERRUPTED).unwrap();
        assert_eq!(s.status, ScanStatus::Failed);
        assert!(s.finished_at.is_some(), "interrupted scans are terminal");
        let reason = s.failure_reason.as_deref().unwrap();
        assert!(reason.starts_with("interrupted:"), "got {reason:?}");
        assert_eq!(s.total_disk_size, 0, "partial results are discarded");
        // Encode keeps the key present with the text.
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["failureReason"], reason);
    }

    /// Pre-1.1 bodies have no `failureReason`; they decode as null, and
    /// re-encoding adds the key present-as-null (wire rule 3).
    #[test]
    fn missing_failure_reason_decodes_as_null_and_encodes_present() {
        let mut v: serde_json::Value = serde_json::from_str(RAW_SCAN_COMPLETE).unwrap();
        v.as_object_mut().unwrap().remove("failureReason");
        let s: Scan = serde_json::from_value(v).unwrap();
        assert_eq!(s.failure_reason, None);
        let out = serde_json::to_value(&s).unwrap();
        assert!(out.as_object().unwrap().contains_key("failureReason"));
        assert!(out["failureReason"].is_null());
    }

    #[test]
    fn null_unreadable_paths_round_trips_as_not_recorded() {
        let mut v: serde_json::Value = serde_json::from_str(RAW_SCAN_COMPLETE).unwrap();
        v["unreadablePaths"] = serde_json::Value::Null;
        let s: Scan = serde_json::from_value(v).unwrap();
        assert_eq!(s.unreadable_paths, None);
        let out = serde_json::to_value(&s).unwrap();
        assert!(out.as_object().unwrap().contains_key("unreadablePaths"));
        assert!(out["unreadablePaths"].is_null());
    }

    #[test]
    fn encodes_camel_case_with_explicit_nulls() {
        let s: Scan = serde_json::from_str(RAW_SCAN_RUNNING).unwrap();
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        let obj = v.as_object().unwrap();
        // Keys are camelCase and nullable fields are present-as-null.
        assert!(obj.contains_key("rootPath"));
        assert!(obj.contains_key("totalDiskSize"));
        assert!(obj.contains_key("totalPrivateSize"));
        assert!(obj.contains_key("totalSharedSize"));
        assert!(obj.contains_key("finishedAt"));
        assert!(obj["finishedAt"].is_null());
        assert_eq!(obj["status"], "running");
        // Canonical datetime: 6 fractional digits, Z form.
        assert_eq!(obj["startedAt"], "2026-03-17T14:30:00.000000Z");
    }

    #[test]
    fn rejects_unknown_status_string() {
        let raw = RAW_SCAN_COMPLETE.replace("\"complete\"", "\"exploded\"");
        assert!(serde_json::from_str::<Scan>(&raw).is_err());
    }

    #[test]
    fn accepts_uppercase_uuid_on_decode() {
        let raw = RAW_SCAN_COMPLETE.replace(
            "e7ae86e2-308b-444c-8a3d-cd21467ab442",
            "E7AE86E2-308B-444C-8A3D-CD21467AB442",
        );
        let s: Scan = serde_json::from_str(&raw).unwrap();
        // Canonical encode is lowercase regardless of input casing.
        assert_eq!(
            serde_json::to_value(&s).unwrap()["id"],
            "e7ae86e2-308b-444c-8a3d-cd21467ab442"
        );
    }

    #[test]
    fn status_round_trips_through_db_strings() {
        for status in [
            ScanStatus::Running,
            ScanStatus::Complete,
            ScanStatus::Cancelled,
            ScanStatus::Failed,
        ] {
            assert_eq!(ScanStatus::from_str(status.as_str()).unwrap(), status);
        }
    }

    #[test]
    fn status_from_unknown_string_is_invalid_input() {
        let err = ScanStatus::from_str("exploded").unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)));
    }

    #[test]
    fn decodes_file_entry_from_raw_bytes() {
        let e: ScanEntry = serde_json::from_str(RAW_ENTRY_FILE).unwrap();
        assert_eq!(e.name, "Cargo.lock");
        assert!(!e.is_dir);
        assert_eq!(e.disk_size, 49152);
        assert_eq!(e.logical_size, 47811);
        assert_eq!(e.file_type.as_deref(), Some("lock"));
        assert_eq!(e.category, None);
        assert_eq!((e.nlink, e.dev, e.ino), (1, 16777233, 42424242));
        assert!(e.modified_at.is_some());
        // File rows never carry descendant counts.
        assert_eq!((e.file_count, e.dir_count), (None, None));
        // The fixture is a pure clone: full allocation on disk, nothing
        // private, a clone-group id, and the two clone flags.
        assert_eq!(e.private_size, Some(0));
        assert_eq!(e.shared_size, Some(49152));
        assert_eq!(e.clone_id, Some(42_424_242));
        assert_eq!(
            e.flags,
            Some(EntryFlags::MAY_SHARE_BLOCKS | EntryFlags::SHARES_ALL_BLOCKS)
        );
    }

    /// Unknown flag strings decode as nothing (a newer server may add one);
    /// the encoder emits flags in the contract's fixed order.
    #[test]
    fn flags_decode_generously_and_encode_in_contract_order() {
        let raw = RAW_ENTRY_FILE.replace(
            r#"["mayShareBlocks", "sharesAllBlocks"]"#,
            r#"["sharesAllBlocks", "fromTheFuture", "dataless", "mayShareBlocks"]"#,
        );
        assert!(raw.contains("fromTheFuture"), "precondition: the flags were rewritten");
        let e: ScanEntry = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            e.flags,
            Some(EntryFlags::MAY_SHARE_BLOCKS | EntryFlags::SHARES_ALL_BLOCKS | EntryFlags::DATALESS)
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v["flags"],
            serde_json::json!(["mayShareBlocks", "sharesAllBlocks", "dataless"]),
            "bit order, unknown dropped"
        );
        // The bitmask round-trips through the SQLite representation and
        // drops bits it does not know.
        let bits = e.flags.unwrap().bits();
        assert_eq!(EntryFlags::from_bits(bits | 0x8000_0000), e.flags.unwrap());
    }

    // Pre-v5 rows: privateSize / sharedSize / flags are null ("not
    // recorded") and decode as None; encode keeps them present-as-null.
    #[test]
    fn null_share_fields_round_trip_as_not_recorded() {
        let mut v: serde_json::Value = serde_json::from_str(RAW_ENTRY_FILE).unwrap();
        for key in ["privateSize", "sharedSize", "cloneId", "flags"] {
            v[key] = serde_json::Value::Null;
        }
        let e: ScanEntry = serde_json::from_value(v).unwrap();
        assert_eq!((e.private_size, e.shared_size, e.clone_id, e.flags), (None, None, None, None));
        let out = serde_json::to_value(&e).unwrap();
        for key in ["privateSize", "sharedSize", "cloneId", "flags"] {
            assert!(out.as_object().unwrap().contains_key(key), "{key} must be present");
            assert!(out[key].is_null(), "{key} must be null");
        }
    }

    #[test]
    fn decodes_root_dir_entry_with_null_fields() {
        let e: ScanEntry = serde_json::from_str(RAW_ENTRY_ROOT_DIR).unwrap();
        assert!(e.is_dir);
        assert_eq!(e.parent_path, None);
        assert_eq!(e.modified_at, None);
        assert_eq!(e.file_type, None);
        // Dir rows carry full-walk descendant counts; the fixture's values
        // deliberately cohere with scan-complete.json (fileCount 4200,
        // dirCount 310 INCLUDING the root) to pin the excluding-self rule.
        assert_eq!((e.file_count, e.dir_count), (Some(4200), Some(309)));
        // The dir row pins the split's populated case and the null cloneId /
        // empty flags case (a directory is never a clone).
        assert_eq!((e.private_size, e.shared_size), (Some(100_000_000), Some(23_456_789)));
        assert_eq!(e.clone_id, None);
        assert_eq!(e.flags, Some(EntryFlags::empty()));
    }

    #[test]
    fn entry_encodes_nullable_fields_as_present_nulls() {
        let e: ScanEntry = serde_json::from_str(RAW_ENTRY_FILE).unwrap();
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        // The FILE row is the null case for the count fields (dir rows carry
        // values — entry-dir.json covers those); the DIR row is the null
        // case for everything else. Between the two fixtures every nullable
        // field is pinned null-and-present.
        for key in ["category", "fileCount", "dirCount"] {
            assert!(obj.contains_key(key), "{key} must be present");
            assert!(obj[key].is_null(), "{key} must be null on a file row");
        }
        let e: ScanEntry = serde_json::from_str(RAW_ENTRY_ROOT_DIR).unwrap();
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        for key in ["parentPath", "modifiedAt", "fileType", "category", "cloneId"] {
            assert!(obj.contains_key(key), "{key} must be present");
            assert!(obj[key].is_null(), "{key} must be null");
        }
        assert_eq!(obj["fileCount"], 4200);
        assert_eq!(obj["dirCount"], 309);
        assert_eq!(obj["flags"], serde_json::json!([]), "no flags is [], never null");
        assert!(obj.contains_key("diskSize"));
        assert!(obj.contains_key("isDir"));
        assert!(obj.contains_key("privateSize"));
        assert!(obj.contains_key("sharedSize"));
    }

    #[test]
    fn scan_request_decodes_camel_case() {
        let r: ScanRequest =
            serde_json::from_str(r#"{"rootPath": "/tmp/haunt"}"#).unwrap();
        assert_eq!(r.root_path, "/tmp/haunt");
    }

    #[test]
    fn scan_request_cross_volumes_defaults_false_and_decodes() {
        let r: ScanRequest = serde_json::from_str(r#"{"rootPath": "/"}"#).unwrap();
        assert!(!r.cross_volumes, "one filesystem unless asked");
        let r: ScanRequest =
            serde_json::from_str(r#"{"rootPath": "/", "crossVolumes": true}"#).unwrap();
        assert!(r.cross_volumes);
        // camelCase only: the snake_case key is an unknown field.
        assert!(serde_json::from_str::<ScanRequest>(r#"{"rootPath": "/", "cross_volumes": true}"#).is_err());
    }

    #[test]
    fn scan_request_phase2_options_default_off_and_decode() {
        let r: ScanRequest = serde_json::from_str(r#"{"rootPath": "/"}"#).unwrap();
        assert_eq!((r.older_than, r.verify_locks, r.tool_estimates), (None, false, false), "nothing runs unless asked");
        let r: ScanRequest = serde_json::from_str(
            r#"{"rootPath": "/", "olderThan": "3M", "verifyLocks": true, "toolEstimates": true}"#,
        )
        .unwrap();
        assert_eq!((r.older_than.as_deref(), r.verify_locks, r.tool_estimates), (Some("3M"), true, true));
        // olderThan is a string the server validates; a number is a type error here.
        assert!(serde_json::from_str::<ScanRequest>(r#"{"rootPath": "/", "olderThan": 90}"#).is_err());
        assert!(serde_json::from_str::<ScanRequest>(r#"{"rootPath": "/", "verify_locks": true}"#).is_err());
    }

    #[test]
    fn scan_request_rejects_unknown_fields() {
        let err = serde_json::from_str::<ScanRequest>(
            r#"{"rootPath": "/tmp/haunt", "maxDepth": 3}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("maxDepth"), "{err}");
    }

    #[test]
    fn scan_request_rejects_snake_case_key() {
        // The wire contract is camelCase; a snake_case key is an unknown
        // field, not a lenient alias.
        assert!(serde_json::from_str::<ScanRequest>(r#"{"root_path": "/tmp"}"#).is_err());
    }

    #[test]
    fn wire_view_progress_shape_is_pinned_by_the_fixtures() {
        // `progress` belongs to the API's wire view, not to `Scan`; the
        // fixtures still pin its shape so no client can drift on it.
        let running: serde_json::Value = serde_json::from_str(RAW_SCAN_RUNNING).unwrap();
        let progress = running["progress"].as_object().unwrap();
        for key in ["filesSeen", "bytesSeen", "currentPath"] {
            assert!(progress.contains_key(key), "progress missing {key}");
        }
        assert!(progress["filesSeen"].is_u64());

        // Terminal: present-as-null, never absent.
        let complete: serde_json::Value = serde_json::from_str(RAW_SCAN_COMPLETE).unwrap();
        let obj = complete.as_object().unwrap();
        assert!(obj.contains_key("progress"));
        assert!(obj["progress"].is_null());
    }

    #[test]
    fn decodes_treemap_layout_from_raw_bytes() {
        let layout: TreemapLayout = serde_json::from_str(RAW_TREEMAP).unwrap();
        assert_eq!(layout.root_path, "/Users/ghost/Code");
        assert_eq!(layout.total_size, 4_194_304);
        assert_eq!(layout.rects.len(), 4);
        let root = &layout.rects[0];
        assert_eq!((root.depth, root.is_dir), (0, true));
        assert_eq!((root.width, root.height), (800.0, 600.0));
        assert!(!root.residual, "real rects carry residual: false");
        let file = &layout.rects[2];
        assert_eq!(file.name, "big.bin");
        assert_eq!(file.size, 1_048_576);
        assert_eq!(file.file_type.as_deref(), Some("bin"));
        assert!(!file.residual);
        // The residual pseudo-tile: the fixture's sizes cohere (4 MiB root,
        // 3 MiB of children, 1 MiB shortfall) and its path is the PARENT's.
        let res = &layout.rects[3];
        assert!(res.residual);
        assert_eq!(res.path, "/Users/ghost/Code");
        assert_eq!(res.name, "smaller files");
        assert_eq!(res.size, 1_048_576);
        assert!(!res.is_dir);
        assert_eq!(res.file_type, None);
        assert_eq!(res.depth, 1);
    }

    #[test]
    fn treemap_encodes_camel_case_at_every_depth() {
        // The wire contract applies at EVERY nesting depth — the nested rect
        // objects, not just the top-level layout.
        let layout: TreemapLayout = serde_json::from_str(RAW_TREEMAP).unwrap();
        let v = serde_json::to_value(&layout).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("rootPath"));
        assert!(obj.contains_key("totalSize"));
        let rect = v["rects"][0].as_object().unwrap();
        for key in ["isDir", "fileType", "path", "depth", "residual"] {
            assert!(rect.contains_key(key), "rect missing {key}");
        }
        assert!(rect["fileType"].is_null(), "nullable present-as-null in nested objects");
        // residual is ALWAYS present — false on real rects, true on the
        // pseudo-tile — never omitted (mutation target: skip_serializing).
        assert_eq!(rect["residual"], false);
        assert_eq!(v["rects"][3]["residual"], true);
    }

    #[test]
    fn new_scan_starts_running_with_zero_totals() {
        let s = Scan::new("/tmp/haunt");
        assert_eq!(s.status, ScanStatus::Running);
        assert_eq!(s.finished_at, None);
        assert_eq!(
            (s.total_disk_size, s.total_logical_size, s.file_count, s.dir_count, s.error_count),
            (0, 0, 0, 0, 0)
        );
    }
}
