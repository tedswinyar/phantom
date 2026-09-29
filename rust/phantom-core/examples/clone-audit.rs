// Explain a totals gap between the v1.0 walk and the clone-aware walk: how
// many bytes did pure-clone groups save, and how many groups were there.
//   cargo run --release -p phantom-core --example clone-audit -- <root>
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use phantom_core::scanner::{ScanProgress, scan_directory};

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: clone-audit <root>"));
    let out = scan_directory(&root, &ScanProgress::new(), &AtomicBool::new(false)).unwrap();
    let mut groups: HashMap<(u64, u64), (u64, u64)> = HashMap::new(); // (members, alloc)
    let mut naive_with_clones: u64 = 0;
    for e in out.entries.iter().filter(|e| !e.is_dir) {
        if let Some(cid) = e.clone_id {
            let g = groups.entry((e.dev, cid)).or_insert((0, e.disk_size));
            g.0 += 1;
            if g.0 > 1 && e.nlink <= 1 {
                naive_with_clones += e.disk_size;
            }
        }
    }
    println!("files={} total_disk={} clone_groups={} clone_members={}",
        out.file_count, out.total_disk_size, groups.len(),
        groups.values().map(|g| g.0).sum::<u64>());
    println!("bytes the v1.0 walk would have charged for the extra clone members: {naive_with_clones}");
    println!("v1.0-equivalent total: {}", out.total_disk_size + naive_with_clones);
}
