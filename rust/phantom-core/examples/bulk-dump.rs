// Dump what getattrlistbulk vends for one directory — a debugging aid for
// the clone/firmlink/mount attributes. `cargo run -p phantom-core --example
// bulk-dump -- <dir>`.
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: bulk-dump <dir>");
    let mut entries = phantom_core::bulk::read_dir_bulk(Path::new(&dir)).expect("read_dir_bulk");
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        println!(
            "{:<20} {:?} err={} dev={} ino={} nlink={} logical={} alloc={} private={:?} clone_id={:?} refcnt={:?} ext={:#x?} flags={:#x} mnt={} firm={}",
            e.name.to_string_lossy(), e.obj_type, e.error, e.dev, e.ino, e.nlink,
            e.logical_size, e.alloc_size, e.private_size, e.clone_id, e.clone_refcnt,
            e.ext_flags, e.flags, e.is_mount_point, e.is_firmlink()
        );
    }
}
