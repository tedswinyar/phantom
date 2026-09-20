// Volume status (v1.1 Phase 3, phantom-mkn.9; Phase 4, phantom-mkn.12 +
// phantom-4p3): the "how bad is it" an agent anchors on before scanning,
// and the "where is System Data?" decomposition after one.
//
// Three read-only sources, no subprocess unless asked:
//
// * `statfs` on the DATA volume — on APFS `df /` reports the sealed system
//   snapshot, not the user's space (CLAUDE.md Gotchas, 2026-08-20: "140Mi
//   avail" vs an actual 21Gi). Its total/free/used are the CONTAINER's:
//   every APFS volume in the container shares one pool, so `usedBytes` is
//   what all of them consume together.
// * `getattrlist(ATTR_VOL_SPACEUSED)` — THIS volume's own consumption
//   (matches `diskutil apfs list` "Capacity Consumed" within MBs; probed
//   2026-09-08). `usedBytes − volumeUsedBytes` is therefore the other
//   volumes in the container (System, Preboot, Recovery, VM) without
//   running diskutil.
// * CoreFoundation `kCFURLVolumeAvailableCapacityForImportantUsageKey` —
//   what Finder shows as "Available", which silently includes purgeable
//   space. `purgeableBytes = important − f_bavail` (probe 2026-09-08:
//   5.06 GB on this Mac; `diskutil info` exposes no purgeable figure, so a
//   subprocess would not have helped).
//
// Time Machine local snapshots are listed only on request (`tmutil`, fixed
// path, bounded) — they are the usual reason a reclaimed gigabyte does not
// show up in `df` for hours. macOS reports no per-snapshot size; the space
// they pin sits inside purgeableBytes. Phantom SUGGESTS `tmutil
// thinlocalsnapshots` and never runs it.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::probe;
use crate::scan::Scan;
use crate::{CoreError, Result};

/// The volume that holds the user's data on a modern macOS install.
pub const DATA_VOLUME: &str = "/System/Volumes/Data";

/// Where user homes live; siblings of the current user's home are the
/// "other users" slice of hidden space.
pub const USERS_DIR: &str = "/Users";

/// tmutil's fixed location; never `$PATH`.
pub const TMUTIL: &str = "/usr/bin/tmutil";
/// Wall-clock bound on the snapshot listing.
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// The byte count the thin-snapshots suggestion names when purgeable space
/// is unknown or zero: 10 GB, tmutil's argument is decimal bytes.
pub const THIN_SUGGESTION_FALLBACK_BYTES: u64 = 10_000_000_000;
/// tmutil's most aggressive urgency (1–4).
pub const THIN_SUGGESTION_URGENCY: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeStatus {
    /// The path asked about.
    pub path: String,
    /// The mount point that holds it.
    pub mount_point: String,
    pub filesystem: String,
    /// Container size (statfs f_blocks × f_bsize).
    pub total_bytes: u64,
    /// total − free: what EVERY volume in the container consumes together.
    pub used_bytes: u64,
    /// Free to root (statfs f_bfree).
    pub free_bytes: u64,
    /// Free to an unprivileged process (f_bavail) — the honest headroom.
    pub available_bytes: u64,
    /// THIS volume's own consumption (`getattrlist ATTR_VOL_SPACEUSED`);
    /// null when the filesystem does not report it. Present-as-null.
    #[serde(default)]
    pub volume_used_bytes: Option<u64>,
    /// Space macOS may reclaim on its own (caches, local snapshots, …):
    /// `importantUsageBytes − availableBytes`, floored at 0. Null when
    /// CoreFoundation has no answer for the volume.
    pub purgeable_bytes: Option<u64>,
    /// Finder's "Available": free space for an important write, purgeable
    /// included (`kCFURLVolumeAvailableCapacityForImportantUsageKey`).
    #[serde(default)]
    pub important_usage_bytes: Option<u64>,
    /// The conservative twin: free space for an opportunistic write
    /// (`kCFURLVolumeAvailableCapacityForOpportunisticUsageKey`).
    #[serde(default)]
    pub opportunistic_usage_bytes: Option<u64>,
    /// Local Time Machine snapshots on the volume; null unless asked.
    pub snapshot_count: Option<u64>,
    /// Their names (`com.apple.TimeMachine.<date>.local`); null unless asked.
    pub snapshots: Option<Vec<String>>,
    /// The used − scanned decomposition. Always an object; the scan-relative
    /// fields are null until a completed scan is named.
    #[serde(default)]
    pub hidden: HiddenSpace,
    pub note: String,
}

/// Where the bytes a scan did not see went. Read-only reporting; the
/// container arithmetic is the reliable figure (Finder, Disk Utility and
/// System Settings disagree with each other).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenSpace {
    /// The completed scan the split is relative to; null when none was named.
    pub scan_id: Option<Uuid>,
    pub scan_root_path: Option<String>,
    /// The scan's `totalDiskSize` — deduped allocated bytes under its root.
    pub scanned_bytes: Option<u64>,
    /// `volumeUsedBytes − scannedBytes` (floored at 0): bytes on THIS volume
    /// the scan did not count — outside its root, other users' homes,
    /// unreadable directories, blocks pinned by snapshots. Null without a
    /// scan or without `volumeUsedBytes`.
    pub unscanned_bytes: Option<u64>,
    /// `usedBytes − volumeUsedBytes` (floored at 0): the other APFS volumes
    /// sharing the container — System, Preboot, Recovery, VM. Null without
    /// `volumeUsedBytes`. Independent of any scan.
    pub other_volumes_bytes: Option<u64>,
    /// Home directories under /Users other than the current user's, with
    /// whether this process could read them. Their sizes are not reported:
    /// an unreadable home cannot be measured, and a readable one is a scan
    /// away. Empty when the volume is not the one holding /Users.
    pub other_user_homes: Vec<UserHome>,
    /// The scan's `errorCount` — entries it could not read; null without a
    /// scan.
    pub unreadable_count: Option<u64>,
    /// A `tmutil thinlocalsnapshots` command line for the USER to run, or
    /// null when snapshots were not listed or none exist. Phantom never
    /// runs it.
    pub snapshot_suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserHome {
    pub path: String,
    /// `access(R_OK | X_OK)` succeeded — the directory could be scanned.
    pub readable: bool,
}

/// Default path when the caller names none: the data volume if it exists
/// (every macOS since Catalina), else `/`.
pub fn default_path() -> &'static str {
    if Path::new(DATA_VOLUME).is_dir() {
        DATA_VOLUME
    } else {
        "/"
    }
}

/// statfs + getattrlist + CoreFoundation, (optionally) the snapshot list,
/// and (optionally) the decomposition relative to a completed scan. The
/// CoreFoundation's capacity answers, or None where it did not really answer.
/// For a user with no GUI session (the build server's `builder`, a LaunchDaemon,
/// an ssh login) `kCFURLVolumeAvailableCapacityForImportantUsageKey` comes back
/// as 0 rather than an error — found the first time the gate ran on the MBP
/// runner, 2026-09-17. Taken at face value that made `purgeableBytes` a
/// confident 0 ("nothing purgeable") when the truth was "no answer", which the
/// contract spells as null. Finder's "Available" can never be below the plain
/// `f_bavail`, so an important-usage figure under it is not an answer; an
/// opportunistic figure of 0 is not one either.
fn sanitize_capacities(
    (important, opportunistic): (Option<u64>, Option<u64>),
    available: u64,
) -> (Option<u64>, Option<u64>) {
    (
        important.filter(|&i| i >= available),
        opportunistic.filter(|&o| o > 0),
    )
}

/// caller has already resolved the scan and checked it is terminal and
/// complete; this only refuses a scan whose root is on another volume.
pub fn volume_status(path: &str, list_snapshots: bool, scan: Option<&Scan>) -> Result<VolumeStatus> {
    let mut status = statfs_status(path)?;
    status.volume_used_bytes = volume_space_used(path);
    let (important, opportunistic) =
        sanitize_capacities(capacity_for_usage(path), status.available_bytes);
    status.important_usage_bytes = important;
    status.opportunistic_usage_bytes = opportunistic;
    status.purgeable_bytes = important.map(|i| i - status.available_bytes);
    if list_snapshots {
        let names = list_local_snapshots(&status.mount_point)?;
        status.snapshot_count = Some(names.len() as u64);
        status.snapshots = Some(names);
    }
    status.hidden = decompose(&status, scan)?;
    Ok(status)
}

/// The arithmetic, separated from the syscalls so it can be pinned on
/// fixture numbers. `other_user_homes` is filled by the caller-visible
/// wrapper because it touches the filesystem.
pub fn decompose(status: &VolumeStatus, scan: Option<&Scan>) -> Result<HiddenSpace> {
    if let Some(scan) = scan
        && let Some(root_mount) = mount_point_of(&scan.root_path)
        && root_mount != status.mount_point
    {
        return Err(CoreError::InvalidInput(format!(
            "scan {} is of {} on volume {root_mount}, not {}; name a path on the scan's volume",
            scan.id, scan.root_path, status.mount_point
        )));
    }
    let mut hidden = decompose_numbers(status, scan);
    hidden.other_user_homes = if mount_point_of(USERS_DIR).as_deref() == Some(status.mount_point.as_str()) {
        other_user_homes(Path::new(USERS_DIR), current_home().as_deref())
    } else {
        Vec::new()
    };
    Ok(hidden)
}

/// Pure: the numbers and the suggestion text from a status and a scan.
pub fn decompose_numbers(status: &VolumeStatus, scan: Option<&Scan>) -> HiddenSpace {
    let volume_used = status.volume_used_bytes;
    let scanned = scan.map(|s| s.total_disk_size);
    HiddenSpace {
        scan_id: scan.map(|s| s.id),
        scan_root_path: scan.map(|s| s.root_path.clone()),
        scanned_bytes: scanned,
        unscanned_bytes: match (volume_used, scanned) {
            (Some(v), Some(s)) => Some(v.saturating_sub(s)),
            _ => None,
        },
        other_volumes_bytes: volume_used.map(|v| status.used_bytes.saturating_sub(v)),
        other_user_homes: Vec::new(),
        unreadable_count: scan.map(|s| s.error_count),
        snapshot_suggestion: match status.snapshot_count {
            Some(n) if n > 0 => Some(thin_snapshots_suggestion(&status.mount_point, status.purgeable_bytes)),
            _ => None,
        },
    }
}

/// The command the user may run. `purgeable` sizes the request when known
/// and non-zero; otherwise the fallback, and the text says to substitute.
pub fn thin_snapshots_suggestion(mount_point: &str, purgeable: Option<u64>) -> String {
    let bytes = match purgeable {
        Some(p) if p > 0 => p,
        _ => THIN_SUGGESTION_FALLBACK_BYTES,
    };
    format!(
        "tmutil thinlocalsnapshots {mount_point} {bytes} {THIN_SUGGESTION_URGENCY}  # asks Time Machine to thin \
         local snapshots until {bytes} bytes are freed (urgency {THIN_SUGGESTION_URGENCY} = most aggressive); replace \
         the byte count with what you want back. Phantom never runs this — run it yourself, then re-check the volume."
    )
}

/// Siblings of `home` under `users_dir`: directories that are not the
/// current home, not `Shared`, not dot-entries. Sorted by path.
pub fn other_user_homes(users_dir: &Path, home: Option<&Path>) -> Vec<UserHome> {
    let Ok(rd) = std::fs::read_dir(users_dir) else {
        return Vec::new();
    };
    let mut homes: Vec<UserHome> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name != "Shared"
        })
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| home.is_none_or(|h| !same_dir(p, h)))
        .map(|p| UserHome {
            readable: is_readable_dir(&p),
            path: p.to_string_lossy().into_owned(),
        })
        .collect();
    homes.sort_by(|a, b| a.path.cmp(&b.path));
    homes
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn current_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(unix)]
fn is_readable_dir(p: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated path; access() reads it and touches
    // nothing else.
    unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::X_OK) == 0 }
}

#[cfg(not(unix))]
fn is_readable_dir(p: &Path) -> bool {
    std::fs::read_dir(p).is_ok()
}

/// statfs `f_bavail` of the volume holding `path` — the headroom a growth
/// forecast divides by. None when the path cannot be stat'd.
pub fn available_bytes(path: &str) -> Option<u64> {
    statfs_status(path).ok().map(|s| s.available_bytes)
}

/// The mount point holding `path`, or None when it cannot be stat'd (a
/// scan root that has since been removed).
fn mount_point_of(path: &str) -> Option<String> {
    statfs_status(path).ok().map(|s| s.mount_point)
}

#[cfg(target_os = "macos")]
fn statfs_status(path: &str) -> Result<VolumeStatus> {
    use std::ffi::{CStr, CString};
    let c_path = CString::new(path)
        .map_err(|_| CoreError::InvalidInput("path contains a NUL byte".into()))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: statfs writes into the zeroed struct we own; the path is a
    // valid NUL-terminated C string for the duration of the call.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(if err.kind() == std::io::ErrorKind::NotFound {
            CoreError::NotFound(format!("path {path}"))
        } else {
            CoreError::Io(err)
        });
    }
    let bsize = st.f_bsize as u64;
    let total = st.f_blocks * bsize;
    let free = st.f_bfree * bsize;
    let avail = st.f_bavail * bsize;
    // SAFETY: the kernel NUL-terminates both fixed-size name buffers.
    let mount_point = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let filesystem = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(VolumeStatus {
        path: path.to_string(),
        mount_point,
        filesystem,
        total_bytes: total,
        used_bytes: total.saturating_sub(free),
        free_bytes: free,
        available_bytes: avail,
        volume_used_bytes: None,
        purgeable_bytes: None,
        important_usage_bytes: None,
        opportunistic_usage_bytes: None,
        snapshot_count: None,
        snapshots: None,
        hidden: HiddenSpace::default(),
        note: note(),
    })
}

#[cfg(not(target_os = "macos"))]
fn statfs_status(_path: &str) -> Result<VolumeStatus> {
    Err(CoreError::InvalidInput(
        "volume status is implemented for macOS (APFS) only".into(),
    ))
}

/// `getattrlist(ATTR_VOL_INFO | ATTR_VOL_SPACEUSED)`: this volume's own
/// consumption. None when the call fails or the filesystem leaves the
/// attribute out (the returned length says so). Requesting a single
/// attribute keeps the buffer layout trivial: `u32 length, u64 used`.
#[cfg(target_os = "macos")]
fn volume_space_used(path: &str) -> Option<u64> {
    use std::ffi::CString;
    #[repr(C, packed)]
    struct Buf {
        len: u32,
        used: u64,
    }
    let c_path = CString::new(path).ok()?;
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.volattr = libc::ATTR_VOL_INFO | libc::ATTR_VOL_SPACEUSED;
    let mut buf = Buf { len: 0, used: 0 };
    // SAFETY: the kernel writes at most `size_of::<Buf>()` bytes into a
    // buffer we own; the path and attrlist are valid for the call.
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            &mut al as *mut libc::attrlist as *mut libc::c_void,
            &mut buf as *mut Buf as *mut libc::c_void,
            std::mem::size_of::<Buf>(),
            0,
        )
    };
    let len = buf.len;
    if rc != 0 || len as usize != std::mem::size_of::<Buf>() {
        return None;
    }
    Some(buf.used)
}

#[cfg(not(target_os = "macos"))]
fn volume_space_used(_path: &str) -> Option<u64> {
    None
}

/// CoreFoundation's two "available for usage" capacities for the volume
/// holding `path`: (important, opportunistic). Each None when the key has
/// no value. No subprocess; no Foundation, only the C API.
#[cfg(target_os = "macos")]
fn capacity_for_usage(path: &str) -> (Option<u64>, Option<u64>) {
    cf::capacity_for_usage(path)
}

#[cfg(not(target_os = "macos"))]
fn capacity_for_usage(_path: &str) -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(target_os = "macos")]
mod cf {
    use std::os::raw::c_void;

    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFURLRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFErrorRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CFIndex = isize;
    type Boolean = u8;
    /// CFNumberType kCFNumberSInt64Type.
    const K_CF_NUMBER_SINT64_TYPE: CFIndex = 4;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFStringRef;
        static kCFURLVolumeAvailableCapacityForOpportunisticUsageKey: CFStringRef;
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: CFAllocatorRef,
            buffer: *const u8,
            buf_len: CFIndex,
            is_directory: Boolean,
        ) -> CFURLRef;
        fn CFURLCopyResourcePropertyForKey(
            url: CFURLRef,
            key: CFStringRef,
            property_value: *mut CFTypeRef,
            error: *mut CFErrorRef,
        ) -> Boolean;
        fn CFNumberGetValue(number: CFNumberRef, the_type: CFIndex, value_ptr: *mut c_void) -> Boolean;
        fn CFRelease(cf: CFTypeRef);
    }

    pub(super) fn capacity_for_usage(path: &str) -> (Option<u64>, Option<u64>) {
        // SAFETY: the URL is created from a byte slice we own and released
        // before returning; each property value is a CFNumber we release
        // after reading. Null checks guard every dereference.
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                path.as_ptr(),
                path.len() as CFIndex,
                1,
            );
            if url.is_null() {
                return (None, None);
            }
            let important = read_i64(url, kCFURLVolumeAvailableCapacityForImportantUsageKey);
            let opportunistic = read_i64(url, kCFURLVolumeAvailableCapacityForOpportunisticUsageKey);
            CFRelease(url);
            (important, opportunistic)
        }
    }

    unsafe fn read_i64(url: CFURLRef, key: CFStringRef) -> Option<u64> {
        let mut value: CFTypeRef = std::ptr::null();
        let mut error: CFErrorRef = std::ptr::null();
        let ok = unsafe { CFURLCopyResourcePropertyForKey(url, key, &mut value, &mut error) };
        if !error.is_null() {
            unsafe { CFRelease(error) };
        }
        if ok == 0 || value.is_null() {
            return None;
        }
        let mut n: i64 = 0;
        let got = unsafe { CFNumberGetValue(value, K_CF_NUMBER_SINT64_TYPE, &mut n as *mut i64 as *mut c_void) };
        unsafe { CFRelease(value) };
        if got == 0 {
            return None;
        }
        u64::try_from(n).ok()
    }
}

fn note() -> String {
    "availableBytes is what an unprivileged process can still write; Finder's 'Available' is \
     importantUsageBytes, which silently adds purgeableBytes (space macOS may reclaim on its own, local \
     Time Machine snapshots included — freed space pinned by a snapshot returns only when it expires; \
     ask with snapshots: true and hidden.snapshotSuggestion names the tmutil command, which Phantom never \
     runs). usedBytes is the whole APFS container; volumeUsedBytes is this volume alone; hidden.otherVolumesBytes \
     is the difference. Name a completed scanId for hidden.unscannedBytes = volumeUsedBytes − scannedBytes. On \
     APFS, `df /` reports the sealed system snapshot — this reads the data volume."
        .to_string()
}

/// `tmutil listlocalsnapshots <mount>` at its fixed path, scrubbed
/// environment, bounded. A missing tmutil (not macOS) or a non-zero exit is
/// an empty list with the reason in the error, never a guess.
fn list_local_snapshots(mount_point: &str) -> Result<Vec<String>> {
    let out = probe::run_bounded(Path::new(TMUTIL), &["listlocalsnapshots", mount_point], None, SNAPSHOT_TIMEOUT)
        .map_err(|e| CoreError::Io(std::io::Error::new(e.kind(), format!("{TMUTIL}: {e}"))))?;
    if out.timed_out {
        return Err(CoreError::InvalidInput(format!(
            "{TMUTIL} listlocalsnapshots did not finish within {}s",
            SNAPSHOT_TIMEOUT.as_secs()
        )));
    }
    if out.status != Some(0) {
        return Err(CoreError::InvalidInput(format!(
            "{TMUTIL} listlocalsnapshots exited {:?}: {}",
            out.status,
            out.stderr.trim()
        )));
    }
    Ok(parse_snapshot_list(&out.stdout))
}

/// tmutil prints a header line (`Snapshots for disk …:` or `Snapshots for
/// volume group …`) followed by one snapshot name per line. Anything that
/// does not look like a snapshot name is skipped, so a wording change in
/// the header cannot become a phantom snapshot.
pub fn parse_snapshot_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("com.apple.TimeMachine.") && l.ends_with(".local"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanStatus;
    use chrono::Utc;

    const RAW_TMUTIL: &str = include_str!("../../../tests/fixtures/probes/tmutil-listlocalsnapshots.txt");
    const RAW_VOLUME: &str = include_str!("../../../tests/fixtures/volume-status.json");

    fn scan_of(root: &str, total: u64, errors: u64) -> Scan {
        Scan {
            id: Uuid::parse_str("5e3c1a2b-8d4f-4c6e-9a1b-2f3d4e5f6a7b").unwrap(),
            root_path: root.into(),
            status: ScanStatus::Complete,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            total_disk_size: total,
            total_logical_size: total,
            file_count: 1,
            dir_count: 1,
            error_count: errors,
            unreadable_paths: Some(vec![]),
            total_private_size: Some(total),
            total_shared_size: Some(0),
            failure_reason: None,
        }
    }

    #[test]
    fn snapshot_list_parses_names_and_ignores_the_header() {
        let names = parse_snapshot_list(RAW_TMUTIL);
        assert_eq!(
            names,
            vec![
                "com.apple.TimeMachine.2026-09-07-220000.local",
                "com.apple.TimeMachine.2026-09-08-090000.local",
                "com.apple.TimeMachine.2026-09-08-100000.local",
            ]
        );
        assert!(parse_snapshot_list("Snapshots for disk /System/Volumes/Data:\n").is_empty());
        assert!(parse_snapshot_list("").is_empty());
        assert!(parse_snapshot_list("garbage\ncom.apple.TimeMachine.x.local.bak\n").is_empty());
    }

    #[test]
    fn volume_fixture_round_trips() {
        let v: VolumeStatus = serde_json::from_str(RAW_VOLUME).unwrap();
        let raw: serde_json::Value = serde_json::from_str(RAW_VOLUME).unwrap();
        assert_eq!(serde_json::to_value(&v).unwrap(), raw);
        assert_eq!(v.snapshot_count, Some(3));
        // The fixture's arithmetic is the contract: purgeable = important −
        // available; otherVolumes = used − volumeUsed; unscanned =
        // volumeUsed − scanned.
        assert_eq!(v.purgeable_bytes, Some(v.important_usage_bytes.unwrap() - v.available_bytes));
        assert_eq!(v.hidden.other_volumes_bytes, Some(v.used_bytes - v.volume_used_bytes.unwrap()));
        assert_eq!(
            v.hidden.unscanned_bytes,
            Some(v.volume_used_bytes.unwrap() - v.hidden.scanned_bytes.unwrap())
        );
        assert!(v.hidden.snapshot_suggestion.as_deref().unwrap().starts_with("tmutil thinlocalsnapshots /System/Volumes/Data "));
        assert_eq!(v.hidden.other_user_homes.len(), 2);
        assert!(!v.hidden.other_user_homes[1].readable);
    }

    /// A pre-Phase-4 body (no volumeUsedBytes / importantUsage / hidden)
    /// still decodes: the additions default to null / empty.
    #[test]
    fn a_phase3_body_decodes_with_the_additions_null() {
        let mut raw: serde_json::Value = serde_json::from_str(RAW_VOLUME).unwrap();
        let obj = raw.as_object_mut().unwrap();
        for k in ["volumeUsedBytes", "importantUsageBytes", "opportunisticUsageBytes", "hidden"] {
            obj.remove(k);
        }
        let v: VolumeStatus = serde_json::from_value(raw).unwrap();
        assert_eq!(v.volume_used_bytes, None);
        assert_eq!(v.hidden, HiddenSpace::default());
    }

    #[test]
    fn decomposition_arithmetic_on_fixture_numbers() {
        let mut v: VolumeStatus = serde_json::from_str(RAW_VOLUME).unwrap();
        let scan = scan_of("/Users/ted", 512_000_000_000, 7);
        let h = decompose_numbers(&v, Some(&scan));
        assert_eq!(h.scan_id, Some(scan.id));
        assert_eq!(h.scan_root_path.as_deref(), Some("/Users/ted"));
        assert_eq!(h.scanned_bytes, Some(512_000_000_000));
        assert_eq!(h.unscanned_bytes, Some(v.volume_used_bytes.unwrap() - 512_000_000_000));
        assert_eq!(h.other_volumes_bytes, Some(v.used_bytes - v.volume_used_bytes.unwrap()));
        assert_eq!(h.unreadable_count, Some(7));
        assert!(h.other_user_homes.is_empty(), "the pure step never touches the filesystem");

        // No scan: scan-relative fields null, the container split stays.
        let h = decompose_numbers(&v, None);
        assert_eq!(h.scan_id, None);
        assert_eq!(h.scanned_bytes, None);
        assert_eq!(h.unscanned_bytes, None);
        assert_eq!(h.unreadable_count, None);
        assert!(h.other_volumes_bytes.is_some());

        // A scan larger than the volume's own usage (a scan that crossed
        // volumes, or a stale number) floors at 0 rather than wrapping.
        let big = scan_of("/", u64::MAX, 0);
        assert_eq!(decompose_numbers(&v, Some(&big)).unscanned_bytes, Some(0));

        // Without volumeUsedBytes, both differences are honest nulls.
        v.volume_used_bytes = None;
        let h = decompose_numbers(&v, Some(&scan));
        assert_eq!(h.unscanned_bytes, None);
        assert_eq!(h.other_volumes_bytes, None);
        assert_eq!(h.scanned_bytes, Some(512_000_000_000), "the scan's own number still reports");
    }

    #[test]
    fn suggestion_only_when_snapshots_were_listed_and_exist() {
        let mut v: VolumeStatus = serde_json::from_str(RAW_VOLUME).unwrap();
        assert!(decompose_numbers(&v, None).snapshot_suggestion.is_some());
        v.snapshot_count = Some(0);
        assert_eq!(decompose_numbers(&v, None).snapshot_suggestion, None, "no snapshots, nothing to thin");
        v.snapshot_count = None;
        assert_eq!(decompose_numbers(&v, None).snapshot_suggestion, None, "not asked, not suggested");
    }

    #[test]
    fn suggestion_sizes_by_purgeable_and_falls_back() {
        let s = thin_snapshots_suggestion("/System/Volumes/Data", Some(5_245_000_000));
        assert!(s.starts_with("tmutil thinlocalsnapshots /System/Volumes/Data 5245000000 4 "), "{s}");
        assert!(s.contains("never runs this"));
        let s = thin_snapshots_suggestion("/", None);
        assert!(s.starts_with(&format!("tmutil thinlocalsnapshots / {THIN_SUGGESTION_FALLBACK_BYTES} 4 ")), "{s}");
        let s = thin_snapshots_suggestion("/", Some(0));
        assert!(s.contains(&format!(" {THIN_SUGGESTION_FALLBACK_BYTES} 4 ")), "zero purgeable is not a request for zero bytes");
    }

    #[test]
    fn other_user_homes_skips_self_shared_and_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        let users = dir.path();
        for name in ["alice", "bob", "Shared", ".localized", "me"] {
            std::fs::create_dir(users.join(name)).unwrap();
        }
        std::fs::write(users.join("notes.txt"), b"a file, not a home").unwrap();
        let homes = other_user_homes(users, Some(&users.join("me")));
        let paths: Vec<&str> = homes.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![users.join("alice").to_str().unwrap(), users.join("bob").to_str().unwrap()]
        );
        assert!(homes.iter().all(|h| h.readable), "a temp dir we made is readable");
        // No known home: every candidate is "other".
        assert_eq!(other_user_homes(users, None).len(), 3);
        // Missing directory: empty, not an error.
        assert!(other_user_homes(&users.join("nope"), None).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_home_is_reported_as_such() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let homes = other_user_homes(dir.path(), None);
        // root ignores modes; anyone else cannot read it.
        let expect = unsafe { libc::geteuid() } == 0;
        assert_eq!(homes, vec![UserHome { path: locked.to_string_lossy().into_owned(), readable: expect }]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn statfs_on_the_root_reports_a_real_volume() {
        let v = volume_status("/", false, None).unwrap();
        assert_eq!(v.path, "/");
        assert_eq!(v.mount_point, "/");
        assert!(v.total_bytes > 0);
        assert!(v.free_bytes <= v.total_bytes);
        assert!(v.available_bytes <= v.free_bytes);
        assert_eq!(v.used_bytes, v.total_bytes - v.free_bytes);
        assert_eq!(v.snapshot_count, None, "not asked, not run");
        assert_eq!(v.snapshots, None);
        assert_eq!(v.hidden.snapshot_suggestion, None);
        assert_eq!(v.hidden.scan_id, None);
        assert!(v.note.contains("purgeableBytes"));
        assert!(!v.filesystem.is_empty());
    }

    /// The two new syscall sources on a real volume: getattrlist's per-volume
    /// usage is at most the container's, and CoreFoundation's important
    /// capacity is at least statfs's available (it adds purgeable).
    #[cfg(target_os = "macos")]
    #[test]
    fn data_volume_reports_its_own_usage_and_purgeable_space() {
        let d = default_path();
        let v = volume_status(d, false, None).unwrap();
        let own = v.volume_used_bytes.expect("APFS reports ATTR_VOL_SPACEUSED");
        assert!(own > 0 && own <= v.used_bytes, "own {own} vs container {}", v.used_bytes);
        assert_eq!(v.hidden.other_volumes_bytes, Some(v.used_bytes - own));
        // CoreFoundation answers for a GUI user (Finder's "Available" ≥
        // f_bavail, purgeable = the difference) and does NOT answer for a
        // headless one (the MBP runner's `builder`: important came back 0,
        // 2026-09-17). Either way the invariant holds: never a confident
        // purgeable figure that CoreFoundation did not stand behind.
        match v.important_usage_bytes {
            Some(important) => {
                assert!(important >= v.available_bytes, "important {important} < available {}", v.available_bytes);
                assert_eq!(v.purgeable_bytes, Some(important - v.available_bytes));
            }
            None => assert_eq!(v.purgeable_bytes, None, "purgeable must be null when CoreFoundation gave no answer"),
        }
        assert_ne!(v.opportunistic_usage_bytes, Some(0), "an opportunistic capacity of 0 is a non-answer, not a number");
        // /Users lives on the data volume, so the other-homes list is
        // computed (possibly empty on a single-user Mac) and never
        // includes our own home.
        let home = std::env::var("HOME").unwrap();
        assert!(v.hidden.other_user_homes.iter().all(|h| h.path != home));
    }

    /// The seam behind the headless case: a CoreFoundation "answer" below
    /// f_bavail is no answer. Mutation: drop the filter (return the tuple
    /// as given) and the first two assertions fail.
    #[test]
    fn capacity_answers_below_free_space_are_no_answer() {
        let avail = 1_521_411_362_816; // the MBP runner's f_bavail, 2026-09-17
        assert_eq!(sanitize_capacities((Some(0), Some(0)), avail), (None, None));
        assert_eq!(sanitize_capacities((Some(avail - 1), Some(5)), avail), (None, Some(5)));
        assert_eq!(sanitize_capacities((Some(avail), Some(5)), avail), (Some(avail), Some(5)));
        assert_eq!(
            sanitize_capacities((Some(avail + 5_060_000_000), Some(avail)), avail),
            (Some(avail + 5_060_000_000), Some(avail))
        );
        assert_eq!(sanitize_capacities((None, None), avail), (None, None));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_scan_on_another_volume_is_refused_and_a_vanished_root_is_tolerated() {
        // /dev is a devfs mount, so a scan rooted there is on another volume.
        let elsewhere = scan_of("/dev", 1, 0);
        let err = volume_status(default_path(), false, Some(&elsewhere)).unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(m) if m.contains("not")));
        // A root that no longer exists cannot be placed; the numbers still
        // report against the volume asked about.
        let gone = scan_of("/no/such/root/anywhere", 4096, 2);
        let v = volume_status(default_path(), false, Some(&gone)).unwrap();
        assert_eq!(v.hidden.scanned_bytes, Some(4096));
        assert_eq!(v.hidden.unreadable_count, Some(2));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_path_prefers_the_data_volume() {
        let d = default_path();
        assert!(d == DATA_VOLUME || d == "/");
        let v = volume_status(d, false, None).unwrap();
        assert!(v.total_bytes > 0);
    }

    #[test]
    fn a_missing_path_is_not_found() {
        assert!(matches!(
            volume_status("/no/such/mount/point/anywhere", false, None),
            Err(CoreError::NotFound(_))
        ));
    }
}
