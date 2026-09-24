// Git work-tree awareness for reclaim plans (phantom-2lz), and the gitignore
// matcher v1.2's exclusion parity (mkn.14) will reuse.
//
// Dogfood 2026-09-09: the safe plan for ~ listed every artifact directory
// under ~/Code/phantom/tests/fixtures/projects (unreal/Intermediate,
// zig/zig-out, maven/target, …) — correctly classified, but COMMITTED. Moving
// them dirtied the repo. A build artifact inside a git work tree is a plan
// candidate only if git ignores it; anything else in a work tree is data.
//
// No subprocess: the `.git` ancestor is found by walking up the path, and
// the rules are read straight from `.gitignore` files (nearest first, the
// way git resolves precedence), `.git/info/exclude`, and the global excludes
// file — via the `ignore` crate's gitignore matcher (ripgrep's).

use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The nearest ancestor of `path` (starting at its parent, so `.git` itself
/// never counts) that holds a `.git` entry — a directory for an ordinary
/// checkout, a file for a linked worktree or a submodule. `None` when
/// `path` is not inside a git work tree.
pub fn work_tree_of(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Is `path` something a reclaim plan must NOT move: inside a git work tree
/// and not ignored by that tree's rules? A committed fixture is data; an
/// ignored `target/` is a cache. Outside any work tree the answer is `false`.
///
/// Precedence follows git: the `.gitignore` nearest the path is consulted
/// first and the first definite verdict (ignore or `!`-whitelist) wins; then
/// each ancestor's `.gitignore` up to the work tree root, then
/// `.git/info/exclude`, then the global excludes file. No verdict at all
/// means not ignored — held back.
pub fn held_back_by_git(path: &Path) -> bool {
    let Some(root) = work_tree_of(path) else {
        return false;
    };
    let is_dir = path.is_dir();
    for dir in path.ancestors().skip(1) {
        let file = dir.join(".gitignore");
        if file.is_file() {
            let (gi, _partial) = Gitignore::new(&file);
            match gi.matched_path_or_any_parents(path, is_dir) {
                Match::Ignore(_) => return false,
                Match::Whitelist(_) => return true,
                Match::None => {}
            }
        }
        if dir == root {
            break;
        }
    }
    let exclude = root.join(".git").join("info").join("exclude");
    if exclude.is_file() {
        let mut b = GitignoreBuilder::new(&root);
        b.add(&exclude);
        if let Ok(gi) = b.build() {
            match gi.matched_path_or_any_parents(path, is_dir) {
                Match::Ignore(_) => return false,
                Match::Whitelist(_) => return true,
                Match::None => {}
            }
        }
    }
    let (global, _) = Gitignore::global();
    // The global matcher has no root under `path`; `matched` (not
    // `…_or_any_parents`) is the call that does not assert containment.
    !matches!(global.matched(path, is_dir), Match::Ignore(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A work tree at `<tmp>/repo`: `.git/` directory, root `.gitignore`
    /// ignoring `/target` and `build/`, a nested `sub/.gitignore` that
    /// re-includes `build`, and the paths the cases below name.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(root.join(".git").join("info")).unwrap();
        fs::write(root.join(".gitignore"), "/target\nbuild/\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("tests/fixtures/maven/target")).unwrap();
        fs::create_dir_all(root.join("sub/build")).unwrap();
        fs::write(root.join("sub/.gitignore"), "!build\n").unwrap();
        fs::create_dir_all(root.join("other/build")).unwrap();
        fs::create_dir_all(root.join("excluded")).unwrap();
        (tmp, root)
    }

    #[test]
    fn finds_the_work_tree_root_for_a_git_directory_or_a_git_file() {
        let (tmp, root) = repo();
        assert_eq!(work_tree_of(&root.join("tests/fixtures/maven/target")), Some(root.clone()));
        assert_eq!(work_tree_of(&root.join("target")), Some(root.clone()));
        // `.git` as a FILE: a linked worktree or a submodule.
        let wt = tmp.path().join("worktree");
        fs::create_dir_all(wt.join("target")).unwrap();
        fs::write(wt.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
        assert_eq!(work_tree_of(&wt.join("target")), Some(wt.clone()));
        // Outside any work tree.
        let loose = tmp.path().join("loose").join("target");
        fs::create_dir_all(&loose).unwrap();
        assert_eq!(work_tree_of(&loose), None);
    }

    /// The dogfood case: a committed fixture's `target/` is not ignored, so
    /// it is held back; the project's own ignored `target/` is a candidate.
    #[test]
    fn ignored_artifacts_are_candidates_and_unignored_paths_in_a_work_tree_are_held_back() {
        let (_tmp, root) = repo();
        assert!(!held_back_by_git(&root.join("target")), "/target is ignored at the root");
        assert!(
            held_back_by_git(&root.join("tests/fixtures/maven/target")),
            "the anchored /target rule does not reach a nested target: committed fixture, held back"
        );
        assert!(!held_back_by_git(&root.join("other/build")), "build/ is ignored anywhere");
    }

    #[test]
    fn the_nearest_gitignore_wins_so_a_whitelist_below_overrides_an_ignore_above() {
        let (_tmp, root) = repo();
        assert!(held_back_by_git(&root.join("sub/build")), "sub/.gitignore says !build");
    }

    #[test]
    fn info_exclude_counts_and_a_tree_with_no_rules_holds_everything_back() {
        let (_tmp, root) = repo();
        assert!(held_back_by_git(&root.join("excluded")));
        fs::write(root.join(".git/info/exclude"), "excluded\n").unwrap();
        assert!(!held_back_by_git(&root.join("excluded")), ".git/info/exclude ignores it");
        // A `.git` file cannot carry info/exclude; with no .gitignore at all
        // every path in that tree is held back.
        let wt = root.parent().unwrap().join("wt2");
        fs::create_dir_all(wt.join("target")).unwrap();
        fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(held_back_by_git(&wt.join("target")));
        fs::write(wt.join(".gitignore"), "target\n").unwrap();
        assert!(!held_back_by_git(&wt.join("target")));
    }

    #[test]
    fn outside_any_work_tree_nothing_is_held_back() {
        let (tmp, _root) = repo();
        let loose = tmp.path().join("loose").join("node_modules");
        fs::create_dir_all(&loose).unwrap();
        assert!(!held_back_by_git(&loose));
        // A path that no longer exists but sits under a work tree: still
        // judged by the rules (the plan may run after a partial move).
        assert!(held_back_by_git(&tmp.path().join("repo").join("gone")));
    }
}
