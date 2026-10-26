// getattrlistbulk(2): one syscall per directory batch instead of one lstat
// per entry, and the ONLY way to read the APFS clone attributes
// (CLONEID / PRIVATESIZE / EXT_FLAGS) that make the headline number exact
// under `cp -c` / Finder Duplicate (phantom-mkn.1). Pure FFI + buffer
// parsing; no walking policy lives here — `scanner.rs` decides what to do
// with each entry.
//
// Buffer layout (getattrlist(2) "ATTRIBUTE BUFFER"): per entry, a u32 total
// length, then the requested attributes in the ORDER the man page lists
// them — common in bit order (RETURNED_ATTRS always first, ERROR second),
// then dir, then file, then the extended-common set (which rides in the
// forkattr slot under FSOPT_ATTR_CMN_EXTENDED). Every value is 4-byte
// aligned, so 64-bit fields may sit at non-8-aligned offsets: every read
// here is `read_unaligned`. An attribute the filesystem does not provide is
// simply NOT packed — the RETURNED_ATTRS bitmap says which fields are
// present, so the cursor must consult it before every read.

#![cfg(target_os = "macos")]

use std::ffi::{CStr, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// sys/attr.h — attrgroup bits.
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

const ATTR_DIR_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_DIR_MOUNTSTATUS: u32 = 0x0000_0004;
const DIR_MNTSTATUS_MNTPOINT: u32 = 0x0000_0001;

const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
const ATTR_CMNEXT_EXT_FLAGS: u32 = 0x0000_0200;
const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;

const FSOPT_NOFOLLOW: u64 = 0x0000_0001;
const FSOPT_ATTR_CMN_EXTENDED: u64 = 0x0000_0020;

const ATTR_BIT_MAP_COUNT: u16 = 5;

// sys/vnode.h — fsobj_type_t values.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

// sys/stat.h — st_flags bits (ATTR_CMN_FLAGS).
pub const UF_COMPRESSED: u32 = 0x0000_0020;
pub const SF_FIRMLINK: u32 = 0x0080_0000;
pub const SF_DATALESS: u32 = 0x4000_0000;

// sys/stat.h — ATTR_CMNEXT_EXT_FLAGS bits.
pub const EF_MAY_SHARE_BLOCKS: u64 = 0x0000_0001;
pub const EF_IS_PURGEABLE: u64 = 0x0000_0008;
pub const EF_IS_SPARSE: u64 = 0x0000_0010;
pub const EF_SHARES_ALL_BLOCKS: u64 = 0x0000_0040;

/// Bytes per bulk call. 256 KiB fits ~2000 entries of our attribute set per
/// syscall; a directory of any size is drained by looping until 0.
const BUF_SIZE: usize = 256 * 1024;

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AttributeSet {
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AttrReference {
    dataoffset: i32,
    length: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: libc::c_int,
        attr_list: *const AttrList,
        attr_buf: *mut libc::c_void,
        attr_buf_size: libc::size_t,
        options: u64,
    ) -> libc::c_int;
}

/// What the entry is, by `ATTR_CMN_OBJTYPE`. Anything that is neither a
/// directory nor a regular file nor a symlink (sockets, fifos, devices) is
/// `Other`: recorded as a zero-byte non-directory, never descended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjType {
    Regular,
    Directory,
    Symlink,
    Other,
}

/// One directory entry as `getattrlistbulk` reports it. `Option` fields are
/// attributes the filesystem did not return (the RETURNED_ATTRS bitmap said
/// so): clone attributes are APFS-only, and a file on HFS+/SMB/exFAT simply
/// has none — that is "unknown", never zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkEntry {
    pub name: OsString,
    pub obj_type: ObjType,
    /// Per-entry error from `ATTR_CMN_ERROR` (an errno), 0 when clean. A
    /// non-zero error means the OTHER fields of this entry are not to be
    /// trusted; the caller counts it as unreadable.
    pub error: u32,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    /// `ATTR_FILE_DATALENGTH` — the data fork's logical length, i.e. st_size.
    pub logical_size: u64,
    /// `ATTR_FILE_ALLOCSIZE` — bytes on disk across all forks, i.e.
    /// st_blocks × 512 on APFS.
    pub alloc_size: u64,
    /// `ATTR_CMNEXT_PRIVATESIZE` — bytes freed immediately if this file were
    /// deleted (not trapped in a clone or snapshot).
    pub private_size: Option<u64>,
    /// `ATTR_CMNEXT_CLONEID` — identifies the data stream; pure clones of one
    /// another share it.
    pub clone_id: Option<u64>,
    /// `ATTR_CMNEXT_CLONE_REFCNT` — how many full clones share all blocks
    /// with this file.
    pub clone_refcnt: Option<u32>,
    /// `ATTR_CMNEXT_EXT_FLAGS` — EF_* bits.
    pub ext_flags: Option<u64>,
    /// `ATTR_CMN_FLAGS` — st_flags (SF_DATALESS, SF_FIRMLINK, …).
    pub flags: u32,
    pub modified_at: Option<SystemTime>,
    /// Directories only: another filesystem is mounted here
    /// (`DIR_MNTSTATUS_MNTPOINT`).
    pub is_mount_point: bool,
}

impl BulkEntry {
    pub fn is_dir(&self) -> bool {
        self.obj_type == ObjType::Directory
    }
    pub fn is_firmlink(&self) -> bool {
        self.flags & SF_FIRMLINK != 0
    }
    pub fn is_dataless(&self) -> bool {
        self.flags & SF_DATALESS != 0
    }
    /// decmpfs (HFS+/APFS transparent compression): the data lives in the
    /// resource fork. The clone attributes then describe an EMPTY data
    /// fork — `private_size` 0, `clone_refcnt` 0 — and must not be read as
    /// "shared with something" (found by the e2e fixture, 2026-09-07).
    pub fn is_compressed(&self) -> bool {
        self.flags & UF_COMPRESSED != 0
    }
    pub fn ext(&self, bit: u64) -> bool {
        self.ext_flags.is_some_and(|f| f & bit != 0)
    }
}

/// The attribute set every bulk read requests. One place, so the parser
/// and the request can never disagree about what is asked for.
fn request() -> AttrList {
    AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME
            | ATTR_CMN_FLAGS
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: ATTR_DIR_LINKCOUNT | ATTR_DIR_MOUNTSTATUS,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH,
        forkattr: ATTR_CMNEXT_PRIVATESIZE
            | ATTR_CMNEXT_CLONEID
            | ATTR_CMNEXT_EXT_FLAGS
            | ATTR_CMNEXT_CLONE_REFCNT,
    }
}

/// Open a directory for bulk reading. With `follow == false`, `O_NOFOLLOW`
/// refuses a symlink at `path` (ENOTDIR/ELOOP) rather than following it —
/// the walker never follows symlinks below the root; `O_DIRECTORY` so a
/// race that swapped a file in fails cleanly.
fn open_dir(path: &Path, follow: bool) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
    // SAFETY: c is a valid NUL-terminated string for the duration of the call.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | nofollow | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedFd(fd))
}

struct OwnedFd(libc::c_int);

impl Drop for OwnedFd {
    fn drop(&mut self) {
        // SAFETY: the fd was returned by open() and is closed exactly once.
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Read every entry of the directory at `path` in one pass. A symlink at
/// `path` is refused, never followed. The order is whatever the filesystem
/// vends (APFS: not sorted); callers sort.
pub fn read_dir_bulk(path: &Path) -> io::Result<Vec<BulkEntry>> {
    read_dir(path, false)
}

/// Like [`read_dir_bulk`] but follows a symlink AT `path` (for a scan root
/// the user named through a link). Entries below are still never followed.
pub fn read_dir_bulk_following(path: &Path) -> io::Result<Vec<BulkEntry>> {
    read_dir(path, true)
}

fn read_dir(path: &Path, follow: bool) -> io::Result<Vec<BulkEntry>> {
    let dir = open_dir(path, follow)?;
    let list = request();
    let mut buf: Vec<u8> = vec![0; BUF_SIZE];
    let mut out = Vec::new();
    loop {
        // SAFETY: buf is BUF_SIZE writable bytes; list outlives the call.
        let n = unsafe {
            getattrlistbulk(
                dir.0,
                &list,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            // The kernel signals a too-small buffer with ERANGE; ours holds
            // hundreds of entries, so a single entry cannot exceed it, but a
            // clean error beats a silent truncation if that ever changes.
            return Err(err);
        }
        if n == 0 {
            break;
        }
        let mut cursor = Cursor { buf: &buf, pos: 0 };
        for _ in 0..n {
            out.push(cursor.parse_entry()?);
        }
    }
    Ok(out)
}

/// A bounds-checked byte cursor over one bulk buffer. Every read is against
/// the entry's declared length, so a malformed length can never read past
/// the buffer; it surfaces as `InvalidData`.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn parse_entry(&mut self) -> io::Result<BulkEntry> {
        let start = self.pos;
        let len = self.read_u32_at(start)? as usize;
        let end = start.checked_add(len).filter(|&e| e <= self.buf.len() && len >= 4).ok_or_else(
            || io::Error::new(io::ErrorKind::InvalidData, "bulk entry length out of bounds"),
        )?;
        let mut f = Field { buf: &self.buf[start..end], pos: 4 };

        // RETURNED_ATTRS is always first; ERROR always second when present.
        let returned: AttributeSet = f.read()?;
        let error = if returned.commonattr & ATTR_CMN_ERROR != 0 {
            f.read::<u32>()?
        } else {
            0
        };
        let name = if returned.commonattr & ATTR_CMN_NAME != 0 {
            let name_ref_pos = f.pos;
            let r: AttrReference = f.read()?;
            f.name_at(name_ref_pos, r)?
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bulk entry without a name"));
        };
        let dev = if returned.commonattr & ATTR_CMN_DEVID != 0 {
            f.read::<i32>()? as u32 as u64
        } else {
            0
        };
        let obj_type = if returned.commonattr & ATTR_CMN_OBJTYPE != 0 {
            match f.read::<u32>()? {
                VREG => ObjType::Regular,
                VDIR => ObjType::Directory,
                VLNK => ObjType::Symlink,
                _ => ObjType::Other,
            }
        } else {
            ObjType::Other
        };
        let modified_at = if returned.commonattr & ATTR_CMN_MODTIME != 0 {
            let ts: Timespec = f.read()?;
            timespec_to_system_time(ts)
        } else {
            None
        };
        let flags = if returned.commonattr & ATTR_CMN_FLAGS != 0 {
            f.read::<u32>()?
        } else {
            0
        };
        let ino = if returned.commonattr & ATTR_CMN_FILEID != 0 {
            f.read::<u64>()?
        } else {
            0
        };

        // Directory attributes come back for directories only; the link
        // count is st_nlink's twin either way (dirs: ATTR_DIR_LINKCOUNT,
        // files: ATTR_FILE_LINKCOUNT below).
        let mut nlink: u64 = 1;
        if returned.dirattr & ATTR_DIR_LINKCOUNT != 0 {
            nlink = f.read::<u32>()? as u64;
        }
        let is_mount_point = if returned.dirattr & ATTR_DIR_MOUNTSTATUS != 0 {
            f.read::<u32>()? & DIR_MNTSTATUS_MNTPOINT != 0
        } else {
            false
        };

        if returned.fileattr & ATTR_FILE_LINKCOUNT != 0 {
            nlink = f.read::<u32>()? as u64;
        }
        let alloc_size = if returned.fileattr & ATTR_FILE_ALLOCSIZE != 0 {
            f.read::<i64>()?.max(0) as u64
        } else {
            0
        };
        let logical_size = if returned.fileattr & ATTR_FILE_DATALENGTH != 0 {
            f.read::<i64>()?.max(0) as u64
        } else {
            0
        };

        let private_size = if returned.forkattr & ATTR_CMNEXT_PRIVATESIZE != 0 {
            Some(f.read::<i64>()?.max(0) as u64)
        } else {
            None
        };
        let clone_id = if returned.forkattr & ATTR_CMNEXT_CLONEID != 0 {
            Some(f.read::<u64>()?)
        } else {
            None
        };
        let ext_flags = if returned.forkattr & ATTR_CMNEXT_EXT_FLAGS != 0 {
            Some(f.read::<u64>()?)
        } else {
            None
        };
        let clone_refcnt = if returned.forkattr & ATTR_CMNEXT_CLONE_REFCNT != 0 {
            Some(f.read::<u32>()?)
        } else {
            None
        };

        self.pos = end;
        Ok(BulkEntry {
            name,
            obj_type,
            error,
            dev,
            ino,
            nlink,
            logical_size,
            alloc_size,
            private_size,
            clone_id,
            clone_refcnt,
            ext_flags,
            flags,
            modified_at,
            is_mount_point,
        })
    }

    fn read_u32_at(&self, at: usize) -> io::Result<u32> {
        let bytes = self.buf.get(at..at + 4).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "bulk buffer truncated")
        })?;
        Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
    }
}

/// Field-by-field reader over ONE entry's bytes, each read 4-byte aligned
/// and bounds-checked against the entry.
struct Field<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Field<'_> {
    fn read<T: Copy>(&mut self) -> io::Result<T> {
        let size = std::mem::size_of::<T>();
        let bytes = self.buf.get(self.pos..self.pos + size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "bulk entry field out of bounds")
        })?;
        // SAFETY: T is a plain-old-data repr(C) type or a primitive; the
        // slice is exactly size_of::<T>() bytes; read_unaligned tolerates
        // the 4-byte alignment the kernel guarantees.
        let v = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) };
        self.pos += size.div_ceil(4) * 4;
        Ok(v)
    }

    /// The name's bytes: `dataoffset` is relative to the attrreference's
    /// own position; `length` includes the NUL.
    fn name_at(&self, ref_pos: usize, r: AttrReference) -> io::Result<OsString> {
        let start = (ref_pos as i64 + r.dataoffset as i64) as usize;
        let bytes = self
            .buf
            .get(start..start + r.length as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bulk name out of bounds"))?;
        let c = CStr::from_bytes_until_nul(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bulk name not NUL-terminated"))?;
        Ok(OsStr::from_bytes(c.to_bytes()).to_os_string())
    }
}

fn timespec_to_system_time(ts: Timespec) -> Option<SystemTime> {
    if ts.sec >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(ts.sec as u64, ts.nsec.clamp(0, 999_999_999) as u32))
    } else {
        UNIX_EPOCH.checked_sub(Duration::new(
            ts.sec.unsigned_abs(),
            ts.nsec.clamp(0, 999_999_999) as u32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn by_name<'a>(entries: &'a [BulkEntry], name: &str) -> &'a BulkEntry {
        entries
            .iter()
            .find(|e| e.name == OsStr::new(name))
            .unwrap_or_else(|| panic!("no entry {name}: {:?}", entries.iter().map(|e| &e.name).collect::<Vec<_>>()))
    }

    /// The parser agrees with lstat on every field both can see. This is
    /// the byte-layout pin: swap any two attributes in `request()` or in
    /// the parse order and ino/dev/size/nlink go wrong here.
    #[test]
    fn bulk_matches_lstat_field_for_field() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), vec![7u8; 5000]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("a.bin", dir.path().join("link")).unwrap();
        fs::hard_link(dir.path().join("a.bin"), dir.path().join("hard")).unwrap();

        let entries = read_dir_bulk(dir.path()).unwrap();
        assert_eq!(entries.len(), 4);
        for e in &entries {
            let md = fs::symlink_metadata(dir.path().join(&e.name)).unwrap();
            assert_eq!(e.error, 0, "{:?}", e.name);
            assert_eq!(e.ino, md.ino(), "ino {:?}", e.name);
            assert_eq!(e.dev, md.dev(), "dev {:?}", e.name);
            assert_eq!(e.is_dir(), md.is_dir(), "{:?}", e.name);
            // Directory link counts are not compared: APFS answers 1 to
            // ATTR_DIR_LINKCOUNT while st_nlink counts subdirectories;
            // nothing downstream reads a directory's nlink.
            if !e.is_dir() {
                assert_eq!(e.nlink, md.nlink(), "nlink {:?}", e.name);
                assert_eq!(e.logical_size, md.len(), "logical {:?}", e.name);
                assert_eq!(e.alloc_size, md.blocks() * 512, "alloc {:?}", e.name);
            }
            let mtime = md.modified().unwrap();
            assert_eq!(e.modified_at, Some(mtime), "mtime {:?}", e.name);
        }
        assert_eq!(by_name(&entries, "link").obj_type, ObjType::Symlink);
        assert_eq!(by_name(&entries, "sub").obj_type, ObjType::Directory);
        assert_eq!(by_name(&entries, "a.bin").obj_type, ObjType::Regular);
        assert_eq!(by_name(&entries, "hard").nlink, 2);
        assert!(!by_name(&entries, "sub").is_mount_point);
    }

    /// APFS clone attributes are present and coherent on a `cp -c` pair:
    /// same CLONEID, both flagged EF_MAY_SHARE_BLOCKS, and the untouched
    /// clone's PRIVATESIZE is 0 while an ordinary copy's equals its
    /// allocation. Skipped-as-FAILURE if the temp volume is not APFS: this
    /// machine class is, and a silently absent attribute would make the
    /// whole feature a no-op.
    #[test]
    fn clone_attributes_are_present_and_coherent_on_apfs() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, vec![9u8; 1 << 20]).unwrap();
        let rc = std::process::Command::new("cp")
            .arg("-c")
            .arg(&a)
            .arg(dir.path().join("b.bin"))
            .status()
            .unwrap();
        assert!(rc.success(), "cp -c must succeed on APFS");
        // NOT fs::copy — on macOS std's copy uses clonefile, so it would be
        // a clone too (the very thing this test distinguishes). Write the
        // same bytes fresh.
        fs::write(dir.path().join("plain.bin"), vec![9u8; 1 << 20]).unwrap();

        let entries = read_dir_bulk(dir.path()).unwrap();
        let a = by_name(&entries, "a.bin");
        let b = by_name(&entries, "b.bin");
        let plain = by_name(&entries, "plain.bin");
        assert!(a.clone_id.is_some(), "CLONEID must be vended: {a:?}");
        assert_eq!(a.clone_id, b.clone_id, "clones share a CLONEID");
        assert_ne!(a.clone_id, plain.clone_id, "a copy is its own stream");
        assert!(a.ext(EF_MAY_SHARE_BLOCKS) && b.ext(EF_MAY_SHARE_BLOCKS));
        assert!(!plain.ext(EF_MAY_SHARE_BLOCKS));
        assert_eq!(a.alloc_size, b.alloc_size, "st_blocks still lies for both");
        assert_eq!(b.private_size, Some(0), "an untouched clone frees nothing");
        assert_eq!(a.private_size, Some(0), "…and neither does its source");
        assert_eq!(plain.private_size, Some(plain.alloc_size));
    }

    #[test]
    fn missing_directory_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_dir_bulk(&dir.path().join("nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        fs::write(&f, "x").unwrap();
        let err = read_dir_bulk(&f).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOTDIR));
    }

    /// A symlink at the directory path is refused, never followed — unless
    /// the caller explicitly asks (the scan root).
    #[test]
    fn symlink_to_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/x"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        let followed = read_dir_bulk_following(&dir.path().join("alias")).unwrap();
        assert_eq!(followed.len(), 1, "following variant lists the target");
        let err = read_dir_bulk(&dir.path().join("alias")).unwrap_err();
        // O_NOFOLLOW|O_DIRECTORY on a symlink: macOS reports ENOTDIR (the
        // link itself is not a directory); ELOOP is the POSIX alternative.
        assert!(
            matches!(err.raw_os_error(), Some(libc::ENOTDIR) | Some(libc::ELOOP)),
            "{err}"
        );
    }

    #[test]
    fn mount_points_are_flagged() {
        // /System/Volumes/Data is a mount point on every macOS ≥ 10.15.
        let entries = read_dir_bulk(Path::new("/System/Volumes")).unwrap();
        let data = by_name(&entries, "Data");
        assert!(data.is_dir());
        assert!(data.is_mount_point, "{data:?}");
    }

    #[test]
    fn firmlinks_are_flagged() {
        // /Users is a firmlink from the sealed system volume to Data.
        let entries = read_dir_bulk(Path::new("/")).unwrap();
        let users = by_name(&entries, "Users");
        assert!(users.is_firmlink(), "{users:?}");
        let system = by_name(&entries, "System");
        assert!(!system.is_firmlink());
    }

    #[test]
    fn empty_directory_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_dir_bulk(dir.path()).unwrap().is_empty());
    }

    /// Thousands of entries drain across several buffer refills.
    #[test]
    fn large_directory_is_fully_drained() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3000 {
            fs::write(dir.path().join(format!("f{i:05}")), "x").unwrap();
        }
        let entries = read_dir_bulk(dir.path()).unwrap();
        assert_eq!(entries.len(), 3000);
        let mut names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 3000, "no entry repeated across refills");
    }
}
