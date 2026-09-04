//! Squarified treemap layout algorithm.
//!
//! Implements the algorithm from Bruls, Huizing, and van Wijk (2000):
//! "Squarified Treemaps" — produces rectangles with aspect ratios
//! as close to 1:1 as possible for better visual readability.

use std::collections::HashMap;

use crate::scan::{ScanEntry, TreemapLayout, TreemapRect};

/// A directory's residual ("smaller files" pseudo-tile) is synthesized only
/// when it is at least this fraction of the directory's own size. Under
/// ADR-0005 almost EVERY directory has some shortfall (any sub-1-MiB file
/// folds into the aggregate without a rect); slivers below half a percent
/// are invisible at any real view size while multiplying the rect count.
/// The boundary is inclusive: exactly 0.5% is emitted.
pub const RESIDUAL_MIN_FRACTION: f64 = 0.005;

/// Display name of the residual pseudo-tile.
pub const RESIDUAL_NAME: &str = "smaller files";

/// Bounding rectangle for layout computation.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn short_side(&self) -> f64 {
        self.w.min(self.h)
    }
}

/// A node in the directory tree used for layout.
#[derive(Debug)]
struct TreeNode {
    path: String,
    name: String,
    size: u64,
    is_dir: bool,
    file_type: Option<String>,
    children: Vec<TreeNode>,
}

/// Compute a treemap layout from scan entries.
///
/// `max_depth` controls how many levels deep to recurse (0 = root only,
/// 1 = root + immediate children, etc.). Pass `usize::MAX` for unlimited.
pub fn layout(
    entries: &[ScanEntry],
    root_path: &str,
    bounds: (f64, f64, f64, f64),
    max_depth: usize,
) -> TreemapLayout {
    let tree = build_tree(entries, root_path);
    let total_size = tree.as_ref().map_or(0, |t| t.size);

    let rect = Rect {
        x: bounds.0,
        y: bounds.1,
        w: bounds.2,
        h: bounds.3,
    };

    let mut rects = Vec::new();
    if let Some(root) = &tree {
        layout_node(root, rect, 0, max_depth, &mut rects);
    }

    TreemapLayout {
        root_path: root_path.to_string(),
        total_size,
        rects,
    }
}

/// Build a tree structure from the flat entry list.
fn build_tree(entries: &[ScanEntry], root_path: &str) -> Option<TreeNode> {
    // Index entries by path
    let by_path: HashMap<&str, &ScanEntry> =
        entries.iter().map(|e| (e.path.as_str(), e)).collect();

    // Group children by parent_path
    let mut children_of: HashMap<&str, Vec<&ScanEntry>> = HashMap::new();
    for entry in entries {
        if let Some(ref parent) = entry.parent_path {
            children_of.entry(parent.as_str()).or_default().push(entry);
        }
    }

    let root_entry = by_path.get(root_path)?;
    Some(build_node(root_entry, &children_of))
}

fn build_node(entry: &ScanEntry, children_of: &HashMap<&str, Vec<&ScanEntry>>) -> TreeNode {
    let children: Vec<TreeNode> = if entry.is_dir {
        children_of
            .get(entry.path.as_str())
            .map(|kids| {
                kids.iter()
                    .map(|child| build_node(child, children_of))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let size = if entry.is_dir {
        // Directory size = sum of children sizes (disk size, aggregated).
        // Persisted directory rows carry their fully aggregated totals
        // (ADR-0005): when the sub-1-MiB remainder was filtered out at
        // persistence time the stored aggregate exceeds the child sum and
        // is the true size. In-memory walks store 0 here, so child_sum wins.
        let child_sum: u64 = children.iter().map(|c| c.size).sum();
        child_sum.max(entry.disk_size).max(1)
    } else {
        // diskSize (actual blocks on disk) — handles sparse files
        entry.disk_size.max(1)
    };

    TreeNode {
        path: entry.path.clone(),
        name: entry.name.clone(),
        size,
        is_dir: entry.is_dir,
        file_type: entry.file_type.clone(),
        children,
    }
}

/// One squarify participant: a real child, or the parent's residual.
enum LayoutItem<'a> {
    Child(&'a TreeNode),
    /// The "smaller files" pseudo-child: the parent's shortfall against its
    /// persisted children (ADR-0005 folding). Never recursed into.
    Residual(u64),
}

impl LayoutItem<'_> {
    fn size(&self) -> u64 {
        match self {
            LayoutItem::Child(c) => c.size,
            LayoutItem::Residual(s) => *s,
        }
    }
}

/// Recursively lay out a node and its children.
fn layout_node(
    node: &TreeNode,
    bounds: Rect,
    depth: usize,
    max_depth: usize,
    rects: &mut Vec<TreemapRect>,
) {
    // Emit a rect for this node
    rects.push(TreemapRect {
        path: node.path.clone(),
        name: node.name.clone(),
        size: node.size,
        x: bounds.x,
        y: bounds.y,
        width: bounds.w,
        height: bounds.h,
        depth,
        is_dir: node.is_dir,
        file_type: node.file_type.clone(),
        residual: false,
    });

    // Stop recursing if we've hit max depth or this is a file. A dir with NO
    // persisted children gets no residual either: its own tile already reads
    // as occupied space — the pseudo-child exists to fill the gap BESIDE
    // real children, and a same-size lone child would just be a duplicate.
    if depth >= max_depth || !node.is_dir || node.children.is_empty() {
        return;
    }

    // Sort children by size descending (required by squarify algorithm)
    let mut items: Vec<LayoutItem> = node.children.iter().map(LayoutItem::Child).collect();

    // Synthesize the residual when the children visibly under-sum the node
    // (node.size is the full-walk aggregate; the difference is the folded
    // sub-1-MiB remainder). It participates in the sort like any child; the
    // stable sort keeps it AFTER equal-sized real children (pushed last).
    let child_sum: u64 = node.children.iter().map(|c| c.size).sum();
    let shortfall = node.size.saturating_sub(child_sum);
    if shortfall > 0 && (shortfall as f64) >= (node.size as f64) * RESIDUAL_MIN_FRACTION {
        items.push(LayoutItem::Residual(shortfall));
    }
    items.sort_by(|a, b| b.size().cmp(&a.size()));

    let total: f64 = items.iter().map(|i| i.size() as f64).sum();
    if total <= 0.0 {
        return;
    }

    // Normalize sizes against the node's own size, not the child sum: for a
    // persisted (ADR-0005-filtered) tree the children cover only part of the
    // directory's true bytes, and inflating them to fill the parent would
    // misrepresent their share. With the residual in the row the item sum
    // equals node.size and coverage is complete; below the residual
    // threshold the sub-half-percent gap stays as parent background.
    let denom = (node.size as f64).max(total);
    // Defensive: node.size ≥ 1 by construction and total > 0 was checked
    // above, so denom > 0 always holds today — but this is the divisor for
    // every child area, and a zero here is NaN geometry in every rect below.
    if denom <= 0.0 {
        return;
    }
    let area = bounds.w * bounds.h;
    let sizes: Vec<f64> = items
        .iter()
        .map(|i| (i.size() as f64 / denom) * area)
        .collect();

    // Run squarify to get sub-rectangles
    let sub_rects = squarify(&sizes, bounds);

    // Recurse into each child; the residual is emitted as a leaf whose path
    // is the PARENT's (a hit on it resolves to the parent directory).
    for (item, sub_rect) in items.iter().zip(sub_rects.iter()) {
        match item {
            LayoutItem::Child(child) => {
                layout_node(child, *sub_rect, depth + 1, max_depth, rects);
            }
            LayoutItem::Residual(size) => {
                rects.push(TreemapRect {
                    path: node.path.clone(),
                    name: RESIDUAL_NAME.to_string(),
                    size: *size,
                    x: sub_rect.x,
                    y: sub_rect.y,
                    width: sub_rect.w,
                    height: sub_rect.h,
                    depth: depth + 1,
                    is_dir: false,
                    file_type: None,
                    residual: true,
                });
            }
        }
    }
}

/// Squarified treemap algorithm.
///
/// Given a list of areas (sorted descending) and a bounding rectangle,
/// produces sub-rectangles that fill the bounds with optimal aspect ratios.
fn squarify(sizes: &[f64], bounds: Rect) -> Vec<Rect> {
    if sizes.is_empty() {
        return Vec::new();
    }

    let mut result = vec![
        Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0
        };
        sizes.len()
    ];
    let mut remaining = bounds;
    let mut start = 0;

    while start < sizes.len() {
        let short = remaining.short_side();
        if short <= 0.0 {
            break;
        }

        // Greedily add items to the current row while aspect ratio improves
        let mut row_sum = 0.0;
        let mut end = start;
        let mut best_ratio = f64::MAX;

        while end < sizes.len() {
            let candidate_sum = row_sum + sizes[end];
            let ratio = worst_ratio(&sizes[start..=end], candidate_sum, short);

            if ratio <= best_ratio {
                best_ratio = ratio;
                row_sum = candidate_sum;
                end += 1;
            } else {
                break;
            }
        }

        // Layout the row [start..end) along the short side. Clamp the
        // fraction to 1.0: float accumulation across rows can leave the
        // last row's sum a hair above the remaining area, and an
        // over-unity fraction would push the strip past the parent bounds.
        let row_fraction = (row_sum / (remaining.w * remaining.h)).min(1.0);
        let is_horizontal = remaining.w >= remaining.h;

        if is_horizontal {
            let row_width = remaining.w * row_fraction;
            let mut y = remaining.y;

            for i in start..end {
                let h = if row_sum > 0.0 {
                    (sizes[i] / row_sum) * remaining.h
                } else {
                    0.0
                };
                result[i] = Rect {
                    x: remaining.x,
                    y,
                    w: row_width,
                    h,
                };
                y += h;
            }

            remaining.x += row_width;
            remaining.w -= row_width;
        } else {
            let row_height = remaining.h * row_fraction;
            let mut x = remaining.x;

            for i in start..end {
                let w = if row_sum > 0.0 {
                    (sizes[i] / row_sum) * remaining.w
                } else {
                    0.0
                };
                result[i] = Rect {
                    x,
                    y: remaining.y,
                    w,
                    h: row_height,
                };
                x += w;
            }

            remaining.y += row_height;
            remaining.h -= row_height;
        }

        start = end;
    }

    result
}

/// Compute the worst (highest) aspect ratio in a row.
fn worst_ratio(row: &[f64], row_sum: f64, short: f64) -> f64 {
    if row.is_empty() || short <= 0.0 || row_sum <= 0.0 {
        return f64::MAX;
    }

    let s2 = short * short;
    let mut worst = 0.0f64;

    for &size in row {
        // aspect ratio = max(w/h, h/w) for each item in the row
        // Using the formula from the paper:
        //   ratio = max(s^2 * r_max / sum^2, sum^2 / (s^2 * r_min))
        // where s = short side, sum = row total
        let ratio = if size > 0.0 {
            let a = (s2 * size) / (row_sum * row_sum);
            let b = (row_sum * row_sum) / (s2 * size);
            a.max(b)
        } else {
            f64::MAX
        };
        worst = worst.max(ratio);
    }

    worst
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(
        name: &str,
        path: &str,
        size: u64,
        is_dir: bool,
        parent: Option<&str>,
    ) -> ScanEntry {
        ScanEntry {
            path: path.to_string(),
            parent_path: parent.map(|s| s.to_string()),
            name: name.to_string(),
            is_dir,
            disk_size: size,
            logical_size: size, // tests use same value for both
            modified_at: None,
            file_type: if is_dir {
                None
            } else {
                Some("txt".to_string())
            },
            category: None,
            nlink: 1,
            dev: 0,
            ino: 0,
            file_count: None,
            dir_count: None,
        }
    }

    #[test]
    fn single_file_fills_bounds() {
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("big.txt", "/root/big.txt", 1000, false, Some("/root")),
        ];

        let result = layout(&entries, "/root", (0.0, 0.0, 800.0, 600.0), 10);
        assert_eq!(result.rects.len(), 2); // root + file
        assert_eq!(result.total_size, 1000);

        let file_rect = &result.rects[1];
        assert_eq!(file_rect.name, "big.txt");
        assert!((file_rect.width - 800.0).abs() < 0.01);
        assert!((file_rect.height - 600.0).abs() < 0.01);
    }

    #[test]
    fn multiple_files_fill_area() {
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("a.txt", "/root/a.txt", 600, false, Some("/root")),
            make_entry("b.txt", "/root/b.txt", 300, false, Some("/root")),
            make_entry("c.txt", "/root/c.txt", 100, false, Some("/root")),
        ];

        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);

        // Should have root + 3 files = 4 rects
        assert_eq!(result.rects.len(), 4);

        // All file rects should be within bounds
        for rect in &result.rects[1..] {
            assert!(rect.x >= -0.01, "x out of bounds: {}", rect.x);
            assert!(rect.y >= -0.01, "y out of bounds: {}", rect.y);
            assert!(
                rect.x + rect.width <= 100.01,
                "right edge out of bounds: {}",
                rect.x + rect.width
            );
            assert!(
                rect.y + rect.height <= 100.01,
                "bottom edge out of bounds: {}",
                rect.y + rect.height
            );
        }

        // Total area of file rects should approximately equal bounding area
        let total_area: f64 = result.rects[1..].iter().map(|r| r.width * r.height).sum();
        assert!(
            (total_area - 10000.0).abs() < 1.0,
            "total area mismatch: {total_area}"
        );
    }

    #[test]
    fn nested_directories() {
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("src", "/root/src", 0, true, Some("/root")),
            make_entry("main.rs", "/root/src/main.rs", 500, false, Some("/root/src")),
            make_entry("lib.rs", "/root/src/lib.rs", 300, false, Some("/root/src")),
            make_entry("readme.txt", "/root/readme.txt", 200, false, Some("/root")),
        ];

        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);

        // root + src dir + 2 src files + readme = 5 rects
        assert_eq!(result.rects.len(), 5);

        // src directory rect should contain its children
        let src_rect = result.rects.iter().find(|r| r.name == "src").unwrap();
        let main_rect = result.rects.iter().find(|r| r.name == "main.rs").unwrap();
        let lib_rect = result.rects.iter().find(|r| r.name == "lib.rs").unwrap();

        assert!(main_rect.x >= src_rect.x - 0.01);
        assert!(main_rect.y >= src_rect.y - 0.01);
        assert!(lib_rect.x >= src_rect.x - 0.01);
        assert!(lib_rect.y >= src_rect.y - 0.01);
    }

    #[test]
    fn max_depth_limits_recursion() {
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("src", "/root/src", 0, true, Some("/root")),
            make_entry("main.rs", "/root/src/main.rs", 500, false, Some("/root/src")),
        ];

        // depth 0 = only root
        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 0);
        assert_eq!(result.rects.len(), 1);

        // depth 1 = root + src (but not src's children)
        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 1);
        assert_eq!(result.rects.len(), 2);

        // depth 2 = everything
        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 2);
        assert_eq!(result.rects.len(), 3);
    }

    #[test]
    fn largest_child_is_laid_out_first_at_the_origin() {
        // Squarify requires size-descending input (Bruls §4): the largest
        // child opens the first row, so its rect anchors at the parent's
        // origin. Flip the sort to ascending and the smallest child sits
        // there instead — this is the test that pins the sort direction.
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("d.txt", "/root/d.txt", 100, false, Some("/root")),
            make_entry("b.txt", "/root/b.txt", 300, false, Some("/root")),
            make_entry("a.txt", "/root/a.txt", 400, false, Some("/root")),
            make_entry("c.txt", "/root/c.txt", 200, false, Some("/root")),
        ];
        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);
        let biggest = result.rects.iter().find(|r| r.name == "a.txt").unwrap();
        assert!(
            biggest.x.abs() < 1e-9 && biggest.y.abs() < 1e-9,
            "largest child must anchor at the origin, got ({}, {})",
            biggest.x,
            biggest.y
        );
    }

    #[test]
    fn layout_uses_disk_size_not_logical_size() {
        // A dataloaded cloud file: huge logical size, ~zero on disk. The
        // treemap must size by disk blocks, or cloud placeholders dominate
        // the view (the exact v0.1 inconsistency this port fixes).
        let mut dataloaded = make_entry(
            "cloud.bin",
            "/root/cloud.bin",
            512, // disk
            false,
            Some("/root"),
        );
        dataloaded.logical_size = 10_000_000;
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            dataloaded,
            make_entry("real.bin", "/root/real.bin", 999_488, false, Some("/root")),
        ];

        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);
        assert_eq!(result.total_size, 1_000_000, "totals are disk bytes");
        let cloud = result.rects.iter().find(|r| r.name == "cloud.bin").unwrap();
        assert!(
            cloud.width * cloud.height < 100.0,
            "dataloaded file must occupy its disk share (<0.1%), got {}",
            cloud.width * cloud.height
        );
    }

    #[test]
    fn persisted_directory_aggregates_override_the_child_sum() {
        // A persisted tree after the ADR-0005 filter: the directory row
        // carries the full 4096-byte aggregate but only a 1024-byte child
        // survived. Two mutation targets pin the seam: drop the
        // `.max(entry.disk_size)` in build_node and the total collapses to
        // 1024; normalize children by child-sum instead of node size in
        // layout_node and the child inflates to fill the whole parent.
        let mut root = make_entry("root", "/root", 0, true, None);
        root.disk_size = 4096;
        let entries = vec![
            root,
            make_entry("kept.bin", "/root/kept.bin", 1024, false, Some("/root")),
        ];

        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);
        assert_eq!(result.total_size, 4096, "stored aggregate is the true size");

        let root_rect = result.rects.iter().find(|r| r.name == "root").unwrap();
        assert_eq!(root_rect.size, 4096);
        let kept = result.rects.iter().find(|r| r.name == "kept.bin").unwrap();
        assert_eq!(kept.size, 1024);
        let share = (kept.width * kept.height) / 10_000.0;
        assert!(
            (share - 0.25).abs() < 0.01,
            "child must occupy its true quarter share, got {share}"
        );
    }

    // -- residual pseudo-tiles ---------------------------------------------

    /// The Ted-smoke scenario: a dir whose aggregate is mostly sub-1-MiB
    /// files. Root stored aggregate 4 MiB; persisted children sum to 3 MiB;
    /// the missing quarter must render as ONE "smaller files" tile, not as
    /// bare parent background.
    fn residual_fixture() -> Vec<ScanEntry> {
        let mut root = make_entry("Code", "/Users/ghost/Code", 0, true, None);
        root.disk_size = 4_194_304;
        let mut sub = make_entry("sub", "/Users/ghost/Code/sub", 0, true, Some("/Users/ghost/Code"));
        sub.disk_size = 2_097_152;
        let mut big = make_entry(
            "big.bin",
            "/Users/ghost/Code/big.bin",
            1_048_576,
            false,
            Some("/Users/ghost/Code"),
        );
        big.file_type = Some("bin".to_string());
        vec![root, sub, big]
    }

    // Mutation targets: compute the shortfall as child_sum - node.size
    // (saturates to 0) and every assertion below fails because no residual
    // is emitted; drop the under-sum guard and full_coverage_emits_no_
    // residual fails instead.
    #[test]
    fn residual_tile_fills_the_shortfall_with_parent_path() {
        let result = layout(&residual_fixture(), "/Users/ghost/Code", (0.0, 0.0, 800.0, 600.0), 10);

        let residuals: Vec<&TreemapRect> =
            result.rects.iter().filter(|r| r.residual).collect();
        assert_eq!(residuals.len(), 1, "exactly one residual: {:?}", result.rects);
        let res = residuals[0];
        assert_eq!(res.size, 1_048_576, "size is node.size - child_sum");
        assert_eq!(res.path, "/Users/ghost/Code", "path is the PARENT dir's");
        assert_eq!(res.name, RESIDUAL_NAME);
        assert!(!res.is_dir);
        assert_eq!(res.file_type, None);
        assert_eq!(res.depth, 1, "parent depth + 1");
        // Geometry: a quarter of the parent's area, placed by squarify like
        // any child (these exact values also pin tests/fixtures/treemap.json).
        assert!((res.x - 400.0).abs() < 1e-9, "x: {}", res.x);
        assert!((res.y - 300.0).abs() < 1e-9, "y: {}", res.y);
        assert!((res.width - 400.0).abs() < 1e-9, "w: {}", res.width);
        assert!((res.height - 300.0).abs() < 1e-9, "h: {}", res.height);
        // Every real rect carries residual: false, present.
        assert!(result.rects.iter().filter(|r| !r.residual).count() == 3);
    }

    #[test]
    fn residual_plus_children_fully_cover_the_parent() {
        // The whole point of the feature: no bare slab. Direct children +
        // the residual tile exactly with the parent's area.
        let result = layout(&residual_fixture(), "/Users/ghost/Code", (0.0, 0.0, 800.0, 600.0), 10);
        let covered: f64 = result
            .rects
            .iter()
            .filter(|r| r.depth == 1)
            .map(|r| r.width * r.height)
            .sum();
        assert!(
            (covered - 800.0 * 600.0).abs() < 1.0,
            "children + residual must cover the parent: {covered}"
        );
    }

    #[test]
    fn full_coverage_emits_no_residual() {
        // Children sum exactly to the node: nothing to synthesize.
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("a.txt", "/root/a.txt", 600, false, Some("/root")),
            make_entry("b.txt", "/root/b.txt", 400, false, Some("/root")),
        ];
        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);
        assert!(
            result.rects.iter().all(|r| !r.residual),
            "no residual at full coverage: {:?}",
            result.rects
        );
    }

    #[test]
    fn residual_threshold_boundary_is_half_a_percent_inclusive() {
        // Exactly 0.5% of the node is emitted; just under is not (the
        // sliver guard — under ADR-0005 almost every dir has SOME shortfall,
        // and emitting them all multiplies the rect count for invisible
        // tiles). Mutation target: change RESIDUAL_MIN_FRACTION and one
        // side of this boundary flips.
        let dir_at = |stored: u64, child: u64, path: &str| {
            let mut root = make_entry("r", &format!("/{path}"), 0, true, None);
            root.disk_size = stored;
            let f = make_entry(
                "kept.bin",
                &format!("/{path}/kept.bin"),
                child,
                false,
                Some(&format!("/{path}")),
            );
            vec![root, f]
        };

        // 200_000 * 0.005 = 1_000 exactly → emitted.
        let at = layout(&dir_at(200_000, 199_000, "at"), "/at", (0.0, 0.0, 100.0, 100.0), 10);
        assert!(
            at.rects.iter().any(|r| r.residual && r.size == 1_000),
            "exact-threshold shortfall must be emitted: {:?}",
            at.rects
        );

        // 999 / 200_000 < 0.5% → suppressed; the gap stays parent background.
        let under = layout(&dir_at(200_000, 199_001, "under"), "/under", (0.0, 0.0, 100.0, 100.0), 10);
        assert!(
            under.rects.iter().all(|r| !r.residual),
            "sub-threshold sliver must not be emitted: {:?}",
            under.rects
        );
    }

    #[test]
    fn recursion_never_descends_into_a_residual() {
        // The residual is a leaf by construction: nothing may be emitted
        // "inside" it. With max_depth high, the residual is the only rect at
        // its depth carrying the parent's path, and no deeper rect exists.
        let result = layout(&residual_fixture(), "/Users/ghost/Code", (0.0, 0.0, 800.0, 600.0), 10);
        let max_emitted = result.rects.iter().map(|r| r.depth).max().unwrap();
        assert_eq!(max_emitted, 1, "sub and big.bin are leaves; residual adds no depth");
        assert_eq!(result.rects.len(), 4, "root + sub + big.bin + residual, nothing more");
    }

    #[test]
    fn childless_dir_gets_no_residual() {
        // A dir whose persisted children were ALL filtered (100% small
        // files) is a leaf tile already reading as occupied space; a
        // same-size lone pseudo-child would duplicate it.
        let mut root = make_entry("root", "/root", 0, true, None);
        root.disk_size = 4096;
        let mut only = make_entry("smalls", "/root/smalls", 0, true, Some("/root"));
        only.disk_size = 4096;
        let result = layout(&vec![root, only], "/root", (0.0, 0.0, 100.0, 100.0), 10);
        assert!(
            result.rects.iter().all(|r| !r.residual),
            "childless dirs stay plain leaves: {:?}",
            result.rects
        );
    }

    #[test]
    fn aspect_ratios_are_reasonable() {
        // With squarified algorithm, we expect decent aspect ratios
        let entries = vec![
            make_entry("root", "/root", 0, true, None),
            make_entry("a.txt", "/root/a.txt", 400, false, Some("/root")),
            make_entry("b.txt", "/root/b.txt", 300, false, Some("/root")),
            make_entry("c.txt", "/root/c.txt", 200, false, Some("/root")),
            make_entry("d.txt", "/root/d.txt", 100, false, Some("/root")),
        ];

        let result = layout(&entries, "/root", (0.0, 0.0, 100.0, 100.0), 10);

        for rect in &result.rects[1..] {
            if rect.width > 0.0 && rect.height > 0.0 {
                let ratio = (rect.width / rect.height).max(rect.height / rect.width);
                assert!(ratio < 10.0, "bad aspect ratio {ratio} for {}", rect.name);
            }
        }
    }
}
