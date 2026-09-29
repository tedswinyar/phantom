// Finished linked git worktrees (phantom-93d): the swarm-sprawl case.
//
// Agent swarms create linked worktrees (`git worktree add`: repo/wt-*,
// repo/.claude/worktrees/agent-*, <pkg>-w1-*, ~/worktrees/*) and never
// remove them. Measured 2026-09-27 on the developer's Mac: 219 linked
// worktrees, 80 of them merged into their default branch and still on disk
// holding 37 GB; node_modules grew 12.8 → 34.6 GB in five days across ~45
// worktrees of 0.3–0.9 GB each. Phantom's hotspot view sees each copy as a
// medium node_modules and nothing tells the user the PARENT is finished
// and can go whole.
//
// Two halves, deliberately separable:
//
// - DETECTION is pure filesystem: a work tree whose `.git` is a FILE
//   reading `gitdir: <repo>/.git/worktrees/<name>` is a linked worktree of
//   `<repo>` ([`linked_worktree_of`]). A submodule's `.git` file points at
//   `.git/modules/<name>` and is not one.
// - STATE needs git: merged = HEAD is an ancestor of a default ref, OR
//   `git cherry <ref> HEAD` prints no `+` line and `<ref>..HEAD` holds no
//   merge commit (rebased/squashed merges — CRUX); clean = `git status
//   --porcelain` is empty ([`worktree_state`]). git runs from a fixed path
//   with the probe runner's scrubbed environment and bound, read-only
//   subcommands only (`--no-optional-locks` keeps `status` from refreshing
//   the index). The same rules as wt-reap, the reaper that acts on them.
//
// Tiers: the regenerable dirs INSIDE a finished worktree are exactly as
// safe as the classifier rated them (they already are plan paths). The
// worktree itself is review — Phantom never deletes source; the actor is
// `git worktree remove` (or a reaper running it for every finished one).
//
// Nothing here touches the wire: [`finished_worktrees`] is computed by a
// client (the CLI's `phantom worktrees`) from the persisted `projects`
// list of a scan's hotspots summary plus a live look at the worktrees.
// Surfacing it on the API/MCP/app is a wire change and its own bead.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::classify::{ProjectActivity, ProjectArtifact};
use crate::probe::{RunOutcome, run_bounded};

/// A work tree registered under another repository's `.git/worktrees/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktree {
    /// The worktree's own root (the directory holding the `.git` file).
    pub root: PathBuf,
    /// `<common>/worktrees/<name>` — the per-worktree git dir.
    pub gitdir: PathBuf,
    /// The repository's common `.git` directory.
    pub common_dir: PathBuf,
    /// The main work tree: the common dir's parent.
    pub main_work_tree: PathBuf,
    /// The worktree's registered name (the last component of `gitdir`).
    pub name: String,
}

/// The nearest ancestor of `path` (including `path` itself) whose `.git`
/// is a FILE pointing into a `worktrees/` directory. `None` for an ordinary
/// checkout (`.git` is a directory), a submodule (`.git/modules/…`), or a
/// path outside any work tree. Filesystem reads only; no git.
pub fn linked_worktree_of(path: &Path) -> Option<LinkedWorktree> {
    let root = path.ancestors().find(|dir| dir.join(".git").exists())?;
    let dotgit = root.join(".git");
    if !dotgit.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dotgit).ok()?;
    let target = text.lines().find_map(|l| l.strip_prefix("gitdir:")).map(str::trim)?;
    let gitdir = absolute(root, Path::new(target));
    // A linked worktree's gitdir is `<common>/worktrees/<name>`.
    let name = gitdir.file_name()?.to_string_lossy().into_owned();
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()?.to_str() != Some("worktrees") {
        return None;
    }
    // Prefer git's own record of the common dir; derive it otherwise.
    let common_dir = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|c| absolute(&gitdir, Path::new(c.trim())))
        .unwrap_or_else(|| worktrees.parent().map(Path::to_path_buf).unwrap_or_default());
    let main_work_tree = common_dir.parent()?.to_path_buf();
    Some(LinkedWorktree {
        root: root.to_path_buf(),
        gitdir,
        common_dir,
        main_work_tree,
        name,
    })
}

/// `rel` against `base` when relative; `.`/`..` resolved lexically.
fn absolute(base: &Path, rel: &Path) -> PathBuf {
    use std::path::Component;
    let joined = if rel.is_absolute() { rel.to_path_buf() } else { base.join(rel) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// What git says about a linked worktree right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeState {
    /// `git status --porcelain` printed nothing (ignored files do not count
    /// — `git worktree remove` deletes them, which is where the space is).
    pub clean: bool,
    /// HEAD is contained in, or patch-equivalent to, a default ref.
    pub merged: bool,
    /// The ref it is merged into, with `(patch-equivalent)` when `git
    /// cherry` rather than ancestry decided it. Null when not merged.
    pub merged_into: Option<String>,
    /// The checked-out branch; null when HEAD is detached.
    pub branch: Option<String>,
    /// The default refs consulted (`origin/HEAD`'s target, else the first
    /// of origin/mainline, origin/main, origin/master, mainline, main,
    /// master that exists; plus origin/mainline whenever it exists).
    pub default_refs: Vec<String>,
    /// Why it is not finished, one clause each: `dirty (3 changes)`,
    /// `unmerged: 2 commit(s) not in origin/main`, `git status failed: …`.
    pub notes: Vec<String>,
}

impl WorktreeState {
    /// Merged AND clean: nothing in it exists only there.
    pub fn finished(&self) -> bool {
        self.merged && self.clean
    }
}

/// The fixed places git may be run from (the probe rule: never `$PATH`).
pub const GIT_CANDIDATES: &[&str] = &["/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git"];

/// The first existing git, or None.
pub fn resolve_git() -> Option<PathBuf> {
    GIT_CANDIDATES.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Runs `git <args>` in `cwd`: the seam tests and the CLI share. `None`
/// means git could not be started.
pub type GitRunner<'a> = &'a dyn Fn(&Path, &[&str]) -> Option<RunOutcome>;

/// A [`GitRunner`] over [`run_bounded`] for a resolved git and a bound.
pub fn git_runner(git: PathBuf, timeout: Duration) -> impl Fn(&Path, &[&str]) -> Option<RunOutcome> {
    move |cwd: &Path, args: &[&str]| run_bounded(&git, args, Some(cwd), timeout).ok()
}

fn ok(out: &Option<RunOutcome>) -> bool {
    matches!(out, Some(RunOutcome { status: Some(0), timed_out: false, .. }))
}

fn first_line(out: &Option<RunOutcome>) -> Option<String> {
    out.as_ref()
        .and_then(|o| o.stdout.lines().next())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
}

/// The default refs a worktree may be merged into (wt-reap's rule).
pub fn default_refs(wt: &LinkedWorktree, run: GitRunner<'_>) -> Vec<String> {
    let cwd = wt.root.as_path();
    let mut refs: Vec<String> = Vec::new();
    let head = run(cwd, &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]);
    if ok(&head) && let Some(r) = first_line(&head) {
        refs.push(r);
    }
    if refs.is_empty() {
        for r in ["origin/mainline", "origin/main", "origin/master", "mainline", "main", "master"] {
            if ok(&run(cwd, &["rev-parse", "-q", "--verify", &format!("{r}^{{commit}}")])) {
                refs.push(r.to_string());
                break;
            }
        }
    }
    if !refs.iter().any(|r| r == "origin/mainline")
        && ok(&run(cwd, &["rev-parse", "-q", "--verify", "origin/mainline^{commit}"]))
    {
        refs.push("origin/mainline".to_string());
    }
    refs
}

/// Ask git about `wt`. Every subcommand is read-only; `status` runs with
/// `--no-optional-locks` so it does not refresh the index either.
pub fn worktree_state(wt: &LinkedWorktree, run: GitRunner<'_>) -> WorktreeState {
    let cwd = wt.root.as_path();
    let mut notes = Vec::new();

    let status = run(cwd, &["--no-optional-locks", "status", "--porcelain"]);
    let clean = match &status {
        Some(o) if o.status == Some(0) && !o.timed_out => {
            let n = o.stdout.lines().filter(|l| !l.trim().is_empty()).count();
            if n > 0 {
                notes.push(format!("dirty ({n} change{})", if n == 1 { "" } else { "s" }));
            }
            n == 0
        }
        Some(o) => {
            let why = o.stderr.lines().next().unwrap_or("").trim();
            notes.push(format!("git status failed: {}", if why.is_empty() { "no output" } else { why }));
            false
        }
        None => {
            notes.push("git status failed: could not start git".to_string());
            false
        }
    };

    let branch = {
        let b = run(cwd, &["symbolic-ref", "-q", "--short", "HEAD"]);
        if ok(&b) { first_line(&b) } else { None }
    };

    let refs = default_refs(wt, run);
    let mut merged_into = None;
    if refs.is_empty() {
        notes.push("no default branch found".to_string());
    } else {
        for r in &refs {
            if ok(&run(cwd, &["merge-base", "--is-ancestor", "HEAD", r])) {
                merged_into = Some(r.clone());
                break;
            }
        }
        if merged_into.is_none() {
            for r in &refs {
                let cherry = run(cwd, &["cherry", r, "HEAD"]);
                let no_plus = ok(&cherry) && !cherry.as_ref().unwrap().stdout.lines().any(|l| l.starts_with('+'));
                if !no_plus {
                    continue;
                }
                let merges = run(cwd, &["rev-list", "--merges", &format!("{r}..HEAD")]);
                if ok(&merges) && first_line(&merges).is_none() {
                    merged_into = Some(format!("{r} (patch-equivalent)"));
                    break;
                }
            }
        }
        if merged_into.is_none() {
            let r = &refs[0];
            let count = run(cwd, &["rev-list", "--count", &format!("{r}..HEAD")]);
            let n = if ok(&count) { first_line(&count).unwrap_or_else(|| "?".into()) } else { "?".into() };
            let where_ = if branch.is_none() { "detached HEAD, " } else { "" };
            notes.push(format!("unmerged: {where_}{n} commit(s) not in {r}"));
        }
    }

    WorktreeState {
        clean,
        merged: merged_into.is_some(),
        merged_into,
        branch,
        default_refs: refs,
        notes,
    }
}

// ---------------------------------------------------------------------------
// Aggregation over a scan's project list

/// One finished linked worktree and the regenerable directories inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishedWorktree {
    pub root: String,
    pub branch: Option<String>,
    pub merged_into: Option<String>,
    /// The hotspot roots directly inside it (the scan's `projects[].artifacts`),
    /// biggest first — these are already plan paths at their own tier.
    pub artifacts: Vec<ProjectArtifact>,
    /// Σ the artifacts' deduped disk bytes.
    pub artifact_disk_size: u64,
}

/// Every finished worktree of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoWorktrees {
    /// The main work tree the worktrees belong to.
    pub main_work_tree: String,
    pub worktrees: Vec<FinishedWorktree>,
    pub count: u64,
    pub artifact_disk_size: u64,
}

/// A linked worktree that is NOT finished, and why — so the count of
/// what is left alone is explicable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldWorktree {
    pub root: String,
    pub main_work_tree: String,
    pub notes: Vec<String>,
    pub artifact_disk_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishedWorktrees {
    /// Repositories with at least one finished worktree, biggest first.
    pub repos: Vec<RepoWorktrees>,
    /// Linked worktrees among the scan's project roots.
    pub linked_worktrees: u64,
    /// …of which merged AND clean.
    pub finished: u64,
    /// Σ artifact bytes across every finished worktree.
    pub artifact_disk_size: u64,
    /// Linked but dirty, unmerged, or unreadable — never suggested.
    pub held: Vec<HeldWorktree>,
    /// Who acts. Human text; wording may change.
    pub actor: String,
}

/// The one sentence every surface prints about who removes a finished
/// worktree. Phantom does not: the worktree is source.
pub const ACTOR: &str = "The build directories inside a finished worktree are plan paths at their own \
    tier already. The worktree itself is review: remove it from its main work tree with `git worktree remove \
    <path>` (then `git worktree prune`) — or run a reaper such as wt-reap (workstation repo, bin/wt-reap), \
    which applies these same merged-and-clean rules to every linked worktree at once. Phantom never deletes source.";

/// Fold a scan's project roots into per-repository finished worktrees.
/// `detect` is [`linked_worktree_of`] (tests may substitute); `state` is
/// [`worktree_state`] bound to a runner. Projects that are not linked
/// worktrees are ignored; the rest are finished or held.
pub fn finished_worktrees(
    projects: &[ProjectActivity],
    detect: &dyn Fn(&Path) -> Option<LinkedWorktree>,
    state: &dyn Fn(&LinkedWorktree) -> WorktreeState,
) -> FinishedWorktrees {
    let mut repos: Vec<RepoWorktrees> = Vec::new();
    let mut held = Vec::new();
    let mut linked = 0_u64;
    let mut finished = 0_u64;
    for project in projects {
        let Some(wt) = detect(Path::new(&project.root)) else { continue };
        // Only the worktree ROOT is the project; an artifact's own nested
        // marker (a package.json inside dist/) must not count it twice.
        if wt.root != Path::new(&project.root) {
            continue;
        }
        linked += 1;
        let mut artifacts = project.artifacts.clone();
        artifacts.sort_by(|a, b| b.disk_size.cmp(&a.disk_size).then_with(|| a.path.cmp(&b.path)));
        let bytes = artifacts.iter().fold(0_u64, |t, a| t.saturating_add(a.disk_size));
        let st = state(&wt);
        let main = wt.main_work_tree.to_string_lossy().into_owned();
        if !st.finished() {
            held.push(HeldWorktree {
                root: project.root.clone(),
                main_work_tree: main,
                notes: st.notes,
                artifact_disk_size: bytes,
            });
            continue;
        }
        finished += 1;
        let entry = FinishedWorktree {
            root: project.root.clone(),
            branch: st.branch,
            merged_into: st.merged_into,
            artifacts,
            artifact_disk_size: bytes,
        };
        match repos.iter_mut().find(|r| r.main_work_tree == main) {
            Some(r) => {
                r.worktrees.push(entry);
                r.count += 1;
                r.artifact_disk_size = r.artifact_disk_size.saturating_add(bytes);
            }
            None => repos.push(RepoWorktrees {
                main_work_tree: main,
                worktrees: vec![entry],
                count: 1,
                artifact_disk_size: bytes,
            }),
        }
    }
    for r in &mut repos {
        r.worktrees
            .sort_by(|a, b| b.artifact_disk_size.cmp(&a.artifact_disk_size).then_with(|| a.root.cmp(&b.root)));
    }
    repos.sort_by(|a, b| {
        b.artifact_disk_size
            .cmp(&a.artifact_disk_size)
            .then_with(|| a.main_work_tree.cmp(&b.main_work_tree))
    });
    held.sort_by(|a, b| b.artifact_disk_size.cmp(&a.artifact_disk_size).then_with(|| a.root.cmp(&b.root)));
    let artifact_disk_size = repos.iter().fold(0_u64, |t, r| t.saturating_add(r.artifact_disk_size));
    FinishedWorktrees {
        repos,
        linked_worktrees: linked,
        finished,
        artifact_disk_size,
        held,
        actor: ACTOR.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Category, RiskTier};
    use std::fs;

    // -- detection: filesystem only ------------------------------------------------

    /// `<tmp>/repo` (an ordinary checkout), `<tmp>/repo/wt-a` (linked, gitdir
    /// relative, with commondir), `<tmp>/elsewhere/wt-b` (linked, gitdir
    /// absolute, no commondir), `<tmp>/repo/sub` (a submodule).
    fn layout() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/wt-a")).unwrap();
        fs::create_dir_all(repo.join(".git/worktrees/wt-b")).unwrap();
        fs::create_dir_all(repo.join(".git/modules/sub")).unwrap();
        fs::write(repo.join(".git/worktrees/wt-a/commondir"), "../..\n").unwrap();
        fs::create_dir_all(repo.join("wt-a/target/debug")).unwrap();
        fs::write(repo.join("wt-a/.git"), "gitdir: ../.git/worktrees/wt-a\n").unwrap();
        let b = tmp.path().join("elsewhere/wt-b");
        fs::create_dir_all(b.join("node_modules")).unwrap();
        fs::write(b.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt-b").display())).unwrap();
        fs::create_dir_all(repo.join("sub/build")).unwrap();
        fs::write(repo.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        fs::create_dir_all(repo.join("target")).unwrap();
        (tmp, repo)
    }

    #[test]
    fn a_git_file_pointing_into_worktrees_is_a_linked_worktree_from_the_root_or_any_path_below() {
        let (tmp, repo) = layout();
        let expect_a = LinkedWorktree {
            root: repo.join("wt-a"),
            gitdir: repo.join(".git/worktrees/wt-a"),
            common_dir: repo.join(".git"),
            main_work_tree: repo.clone(),
            name: "wt-a".into(),
        };
        assert_eq!(linked_worktree_of(&repo.join("wt-a")), Some(expect_a.clone()), "the root itself");
        assert_eq!(linked_worktree_of(&repo.join("wt-a/target")), Some(expect_a.clone()), "an artifact inside");
        assert_eq!(linked_worktree_of(&repo.join("wt-a/target/debug")), Some(expect_a));
        // Absolute gitdir, no commondir: the common dir is derived.
        let b = linked_worktree_of(&tmp.path().join("elsewhere/wt-b/node_modules")).expect("wt-b");
        assert_eq!(b.common_dir, repo.join(".git"));
        assert_eq!(b.main_work_tree, repo);
        assert_eq!(b.name, "wt-b");
        assert_eq!(b.root, tmp.path().join("elsewhere/wt-b"));
    }

    #[test]
    fn an_ordinary_checkout_a_submodule_and_a_loose_directory_are_not_linked_worktrees() {
        let (tmp, repo) = layout();
        assert_eq!(linked_worktree_of(&repo.join("target")), None, ".git is a directory");
        assert_eq!(linked_worktree_of(&repo), None);
        assert_eq!(linked_worktree_of(&repo.join("sub/build")), None, "a submodule's .git file names modules/, not worktrees/");
        let loose = tmp.path().join("loose/target");
        fs::create_dir_all(&loose).unwrap();
        assert_eq!(linked_worktree_of(&loose), None);
        // A .git file with junk in it.
        let junk = tmp.path().join("junk");
        fs::create_dir_all(&junk).unwrap();
        fs::write(junk.join(".git"), "not a gitdir line\n").unwrap();
        assert_eq!(linked_worktree_of(&junk), None);
    }

    // -- state: real git in a temp repo -----------------------------------------------

    fn git() -> PathBuf {
        resolve_git().expect("git at a fixed path is required for these tests")
    }

    /// A bare-ish origin with `main`, a clone as the main work tree, and
    /// linked worktrees in the states the cases need. Returns (tmp, clone).
    fn repo_with_worktrees() -> (tempfile::TempDir, PathBuf, Box<dyn Fn(&Path, &[&str]) -> Option<RunOutcome>>) {
        let tmp = tempfile::tempdir().unwrap();
        // git records gitdir paths canonically (/private/var/… on macOS);
        // work from the canonical tempdir so the comparisons are exact.
        let base = tmp.path().canonicalize().unwrap();
        let run = git_runner(git(), Duration::from_secs(20));
        let sh = |cwd: &Path, args: &[&str]| {
            let out = run(cwd, args).unwrap_or_else(|| panic!("git {args:?} did not start"));
            assert_eq!(out.status, Some(0), "git {args:?} failed: {}{}", out.stdout, out.stderr);
            out.stdout
        };
        let origin = base.join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        sh(&origin, &["init", "-q", "--bare", "--initial-branch=main"]);
        let clone = base.join("clone");
        fs::create_dir_all(&clone).unwrap();
        sh(&clone, &["init", "-q", "--initial-branch=main"]);
        sh(&clone, &["config", "user.email", "t@example.invalid"]);
        sh(&clone, &["config", "user.name", "t"]);
        sh(&clone, &["config", "commit.gpgsign", "false"]);
        fs::write(clone.join("README"), "one\n").unwrap();
        sh(&clone, &["add", "README"]);
        sh(&clone, &["commit", "-q", "-m", "one"]);
        sh(&clone, &["remote", "add", "origin", origin.to_str().unwrap()]);
        sh(&clone, &["push", "-q", "-u", "origin", "main"]);
        (tmp, clone, Box::new(run))
    }

    fn add_worktree(clone: &Path, sh: &dyn Fn(&Path, &[&str]) -> Option<RunOutcome>, name: &str) -> LinkedWorktree {
        let path = clone.join(name);
        let out = sh(clone, &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()]).unwrap();
        assert_eq!(out.status, Some(0), "{}", out.stderr);
        // The worktree's own commits need an identity too (config is shared, set above).
        linked_worktree_of(&path).expect("a real linked worktree detects")
    }

    fn commit(sh: &dyn Fn(&Path, &[&str]) -> Option<RunOutcome>, wt: &Path, file: &str, msg: &str) {
        fs::write(wt.join(file), format!("{msg}\n")).unwrap();
        assert_eq!(sh(wt, &["add", file]).unwrap().status, Some(0));
        let out = sh(wt, &["commit", "-q", "-m", msg]).unwrap();
        assert_eq!(out.status, Some(0), "{}", out.stderr);
    }

    #[test]
    fn detection_matches_gits_own_view_of_a_real_worktree() {
        let (_tmp, clone, run) = repo_with_worktrees();
        let wt = add_worktree(&clone, &*run, "wt-real");
        assert_eq!(wt.main_work_tree, clone);
        assert_eq!(wt.common_dir, clone.join(".git"));
        assert_eq!(wt.name, "wt-real");
        // git agrees about the common dir.
        let common = run(&wt.root, &["rev-parse", "--git-common-dir"]).unwrap().stdout.trim().to_string();
        assert_eq!(absolute(&wt.root, Path::new(&common)), wt.common_dir);
    }

    /// The four states a swarm leaves behind, each judged the way wt-reap
    /// judges it. Mutations that fail this: dropping `--is-ancestor`
    /// (nothing is merged), inverting the `+` test in the cherry branch
    /// (the rebased worktree reads unmerged), counting ignored files as
    /// dirty (the clean worktree reads dirty).
    #[test]
    fn merged_clean_worktrees_are_finished_and_dirty_or_unmerged_ones_are_held_with_reasons() {
        let (_tmp, clone, run) = repo_with_worktrees();
        let sh = &*run;

        // 1. Fresh from main, nothing done: merged (HEAD == origin/main) + clean → finished.
        let idle = add_worktree(&clone, sh, "wt-idle");
        // An IGNORED build dir does not make it dirty.
        fs::write(clone.join(".git/info/exclude"), "target/\n").unwrap();
        fs::create_dir_all(idle.root.join("target")).unwrap();
        fs::write(idle.root.join("target/blob"), b"x").unwrap();
        let st = worktree_state(&idle, sh);
        assert!(st.finished(), "{st:?}");
        assert_eq!(st.merged_into.as_deref(), Some("origin/main"));
        assert_eq!(st.branch.as_deref(), Some("wt-idle"));
        assert!(st.notes.is_empty(), "{st:?}");

        // 2. A commit not on main: unmerged, held.
        let ahead = add_worktree(&clone, sh, "wt-ahead");
        commit(sh, &ahead.root, "a.txt", "ahead");
        let st = worktree_state(&ahead, sh);
        assert!(!st.finished() && st.clean && !st.merged, "{st:?}");
        assert_eq!(st.notes, ["unmerged: 1 commit(s) not in origin/main"]);

        // 3. Merged via a FAST-FORWARD of main to it: ancestor → finished.
        assert_eq!(sh(&clone, &["merge", "-q", "--ff-only", "wt-ahead"]).unwrap().status, Some(0));
        assert_eq!(sh(&clone, &["push", "-q", "origin", "main"]).unwrap().status, Some(0));
        let st = worktree_state(&ahead, sh);
        assert!(st.finished(), "{st:?}");
        assert_eq!(st.merged_into.as_deref(), Some("origin/main"));

        // 4. Merged by REBASE (a different sha for the same patch — the CRUX
        //    shape): not an ancestor, but git cherry finds no '+'.
        let rebased = add_worktree(&clone, sh, "wt-rebased");
        commit(sh, &rebased.root, "r.txt", "rebased work");
        // main moves on first (so the replayed patch gets a different parent
        // and therefore a different sha — a cherry-pick whose parent is the
        // original's parent, in the same second, reproduces the SAME object),
        // then the same patch lands on main as a new commit.
        commit(sh, &clone, "m.txt", "main moves on");
        assert_eq!(sh(&clone, &["cherry-pick", "wt-rebased"]).unwrap().status, Some(0));
        assert_eq!(sh(&clone, &["push", "-q", "origin", "main"]).unwrap().status, Some(0));
        let st = worktree_state(&rebased, sh);
        assert!(st.finished(), "{st:?}");
        assert_eq!(st.merged_into.as_deref(), Some("origin/main (patch-equivalent)"));

        // 5. Merged but DIRTY: held, with the change count.
        fs::write(rebased.root.join("r.txt"), "edited after merge\n").unwrap();
        fs::write(rebased.root.join("new.txt"), "untracked\n").unwrap();
        let st = worktree_state(&rebased, sh);
        assert!(!st.finished() && st.merged && !st.clean, "{st:?}");
        assert_eq!(st.notes, ["dirty (2 changes)"]);

        // 6. Detached HEAD with a commit on no branch: unmerged, and the note says detached.
        let detached = add_worktree(&clone, sh, "wt-detached");
        assert_eq!(sh(&detached.root, &["checkout", "-q", "--detach"]).unwrap().status, Some(0));
        commit(sh, &detached.root, "d.txt", "on no branch");
        let st = worktree_state(&detached, sh);
        assert!(!st.finished() && st.branch.is_none(), "{st:?}");
        assert_eq!(st.notes, ["unmerged: detached HEAD, 1 commit(s) not in origin/main"]);

        // origin/HEAD is unset in this clone, so the fallback list picked origin/main.
        assert_eq!(st.default_refs, ["origin/main"]);
    }

    #[test]
    fn a_worktree_git_cannot_read_is_held_and_says_so() {
        let (_tmp, clone, run) = repo_with_worktrees();
        // A directory that LOOKS like a worktree but whose gitdir is gone.
        let ghost = clone.parent().unwrap().join("ghost");
        fs::create_dir_all(&ghost).unwrap();
        fs::write(ghost.join(".git"), format!("gitdir: {}/nowhere/.git/worktrees/ghost\n", ghost.parent().unwrap().display())).unwrap();
        let wt = linked_worktree_of(&ghost).unwrap();
        let st = worktree_state(&wt, &*run);
        assert!(!st.finished() && !st.clean && !st.merged, "{st:?}");
        assert!(st.notes[0].starts_with("git status failed: "), "{st:?}");
        // A runner that cannot start git at all.
        let none: GitRunner<'_> = &|_, _| None;
        let st = worktree_state(&wt, none);
        assert_eq!(st.notes, ["git status failed: could not start git", "no default branch found"]);
    }

    // -- aggregation: pure over the project list -------------------------------------------

    fn project(root: &str, artifacts: &[(&str, &str, u64)]) -> ProjectActivity {
        ProjectActivity {
            root: root.into(),
            last_activity_days: Some(3),
            dormant: false,
            artifacts: artifacts
                .iter()
                .map(|(rule, path, size)| ProjectArtifact {
                    rule_id: (*rule).into(),
                    path: (*path).into(),
                    category: Category::RegenerableArtifact,
                    risk_tier: RiskTier::Safe,
                    disk_size: *size,
                })
                .collect(),
        }
    }

    fn fake_wt(root: &str, main: &str) -> LinkedWorktree {
        LinkedWorktree {
            root: root.into(),
            gitdir: format!("{main}/.git/worktrees/x").into(),
            common_dir: format!("{main}/.git").into(),
            main_work_tree: main.into(),
            name: "x".into(),
        }
    }

    fn state(finished: bool) -> WorktreeState {
        WorktreeState {
            clean: finished,
            merged: true,
            merged_into: Some("origin/main".into()),
            branch: Some("b".into()),
            default_refs: vec!["origin/main".into()],
            notes: if finished { vec![] } else { vec!["dirty (1 change)".into()] },
        }
    }

    #[test]
    fn finished_worktrees_group_per_repo_biggest_first_and_hold_the_rest_with_reasons() {
        let projects = vec![
            project("/r/A", &[("cargo-target", "/r/A/target", 50)]),
            project("/r/A/wt-1", &[("cargo-target", "/r/A/wt-1/target", 300), ("node-modules", "/r/A/wt-1/node_modules", 100)]),
            project("/r/A/wt-2", &[("cargo-target", "/r/A/wt-2/target", 700)]),
            project("/r/A/wt-dirty", &[("cargo-target", "/r/A/wt-dirty/target", 900)]),
            project("/w/B-w1", &[("node-modules", "/w/B-w1/node_modules", 2000)]),
            // A nested marker inside a worktree's artifact: not a second worktree.
            project("/w/B-w1/dist", &[("js-dist", "/w/B-w1/dist/dist", 5)]),
        ];
        let detect = |p: &Path| -> Option<LinkedWorktree> {
            match p.to_str().unwrap() {
                "/r/A/wt-1" | "/r/A/wt-2" | "/r/A/wt-dirty" => Some(fake_wt(p.to_str().unwrap(), "/r/A")),
                "/w/B-w1" | "/w/B-w1/dist" => Some(fake_wt("/w/B-w1", "/w/B")),
                _ => None,
            }
        };
        let st = |wt: &LinkedWorktree| state(!wt.root.ends_with("wt-dirty"));
        let f = finished_worktrees(&projects, &detect, &st);
        assert_eq!((f.linked_worktrees, f.finished), (4, 3));
        assert_eq!(f.artifact_disk_size, 400 + 700 + 2000);
        let repos: Vec<(&str, u64, u64)> = f.repos.iter().map(|r| (r.main_work_tree.as_str(), r.count, r.artifact_disk_size)).collect();
        assert_eq!(repos, [("/w/B", 1, 2000), ("/r/A", 2, 1100)], "biggest repo first");
        let a = &f.repos[1];
        assert_eq!(a.worktrees[0].root, "/r/A/wt-2", "biggest worktree first");
        assert_eq!(a.worktrees[1].artifacts[0].path, "/r/A/wt-1/target", "artifacts biggest first");
        assert_eq!(a.worktrees[1].merged_into.as_deref(), Some("origin/main"));
        assert_eq!(f.held.len(), 1);
        assert_eq!((f.held[0].root.as_str(), f.held[0].artifact_disk_size), ("/r/A/wt-dirty", 900));
        assert_eq!(f.held[0].notes, ["dirty (1 change)"]);
        assert!(f.actor.contains("git worktree remove") && f.actor.contains("wt-reap") && f.actor.contains("never deletes"));
        // No worktrees at all: an honest empty answer.
        let none = finished_worktrees(&projects, &|_| None, &st);
        assert_eq!(none.repos, Vec::<RepoWorktrees>::new());
        assert_eq!((none.linked_worktrees, none.finished, none.artifact_disk_size), (0, 0, 0));
        assert!(none.held.is_empty());
    }

    #[test]
    fn the_wire_shape_of_the_client_side_report_is_camel_case() {
        let f = finished_worktrees(
            &[project("/r/A/wt-1", &[("cargo-target", "/r/A/wt-1/target", 1)])],
            &|p| Some(fake_wt(p.to_str().unwrap(), "/r/A")),
            &|_| state(true),
        );
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["repos"][0]["mainWorkTree"], "/r/A");
        assert_eq!(v["repos"][0]["worktrees"][0]["artifactDiskSize"], 1);
        assert_eq!(v["repos"][0]["worktrees"][0]["mergedInto"], "origin/main");
        assert_eq!(v["linkedWorktrees"], 1);
        let back: FinishedWorktrees = serde_json::from_value(v).unwrap();
        assert_eq!(back, f);
    }
}
