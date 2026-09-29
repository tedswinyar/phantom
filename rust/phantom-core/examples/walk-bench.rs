// G1 spike benchmark (phantom-mkn.1): the v1.0 jwalk+lstat walk (kept here
// verbatim as `legacy`) versus the shipping getattrlistbulk walker over the
// same root. Prints wall time and totals for both; totals must be identical
// on a clone-free, quiescent tree.
//
//   cargo run --release -p phantom-core --example walk-bench -- <root> [runs]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use phantom_core::format::LinkCharger;
use phantom_core::scanner::{ScanProgress, scan_directory};

#[derive(Debug, Default, PartialEq)]
struct Totals {
    disk: u64,
    logical: u64,
    files: u64,
    dirs: u64,
    errors: u64,
}

/// The v1.0 walk: jwalk (parallel readdir) + one symlink_metadata per entry.
fn legacy(root: &Path) -> Totals {
    let mut t = Totals::default();
    let mut links = LinkCharger::new();
    for entry in jwalk::WalkDir::new(root).skip_hidden(false).sort(true) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                t.errors += 1;
                continue;
            }
        };
        if entry.read_children_error.is_some() {
            t.errors += 1;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                t.errors += 1;
                continue;
            }
        };
        if md.is_dir() {
            t.dirs += 1;
        } else {
            t.files += 1;
            if links.charges(md.nlink(), md.dev(), md.ino(), None) {
                t.disk += md.blocks().saturating_mul(512);
                t.logical += md.len();
            }
        }
    }
    t
}

fn bulk(root: &Path) -> Totals {
    let out = scan_directory(root, &ScanProgress::new(), &AtomicBool::new(false)).unwrap();
    Totals {
        disk: out.total_disk_size,
        logical: out.total_logical_size,
        files: out.file_count,
        dirs: out.dir_count,
        errors: out.error_count,
    }
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: walk-bench <root> [runs]"));
    let runs: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    println!("root: {}  runs: {runs}", root.display());
    let mut results: Vec<(&str, f64, Totals)> = Vec::new();
    for (name, f) in [("legacy", legacy as fn(&Path) -> Totals), ("bulk", bulk)] {
        let mut best = f64::MAX;
        let mut totals = Totals::default();
        for _ in 0..runs {
            let start = Instant::now();
            totals = f(&root);
            best = best.min(start.elapsed().as_secs_f64());
        }
        println!(
            "{name:<7} best {best:8.3}s  disk={} logical={} files={} dirs={} errors={}",
            totals.disk, totals.logical, totals.files, totals.dirs, totals.errors
        );
        results.push((name, best, totals));
    }
    let same = results[0].2 == results[1].2;
    println!(
        "totals identical: {same}   speedup: {:.2}x",
        results[0].1 / results[1].1
    );
}
