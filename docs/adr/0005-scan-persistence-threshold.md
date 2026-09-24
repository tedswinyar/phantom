# ADR-0005: Persist directories and large files, not the full walk

- **Status**: accepted
- **Date**: 2026-08-31

## Context

The v1.0 plan made Phase 1 produce a load-bearing measurement before Phase 2
could commit to a persistence design: round-trip a 100,000-entry synthetic
scan through the store and let the numbers decide between persisting every
entry and a threshold fallback (all directories + files ≥ 1 MiB, with
per-directory rollups of the small remainder).

The measurement (release build, WAL checkpointed; reproduce with
`cargo test -p phantom-core measure_100k -- --ignored --nocapture`):

| Metric | 100k entries |
|---|---|
| Insert | 551 ms |
| Read-back | 88 ms |
| DB file size | 56.3 MB (~590 bytes/entry) |

Wall time passes with room to spare — even a 5M-entry walk inserts in ~30 s,
and inserts happen on scan completion inside an already-async lifecycle. Size
fails. Developer Macs — the audience — routinely carry 1–5 million filesystem
entries (`node_modules`, tool caches, agent worktrees). At ~590 bytes/entry
that is 0.6–3 GB **per scan**, multiplied by keep-last-N retention. A tool
whose headline feature is reclaiming disk space cannot itself be a
multi-gigabyte SQLite file; it would appear in its own Reclaimable view.

## Decision

Adopt the threshold fallback. On scan completion Phantom persists:

- **Every directory**, with its fully aggregated totals (disk, logical,
  counts) — the treemap, the tree view, and drill-down all read directory
  rows, so they lose nothing.
- **Every file whose `diskSize` ≥ 1 MiB** (1,048,576 bytes, a named constant
  in `phantom-core`; the boundary is inclusive and boundary-tested).
- **Per-scan aggregate summaries** for what the omitted rows would have
  answered — at minimum totals by `fileType` (feeding `types` /
  `get_space_by_type`), computed from the full in-memory walk before the
  filter is applied.

Small files still count: their bytes are present in every ancestor
directory's totals and in the scan's totals — only their individual rows are
omitted. The filter is a post-pass at persistence time; the in-flight scan
registry (Phase 2) holds the full walk in memory, so live progress and
completion summaries see everything.

Because nothing has shipped, the summary table joins the **v1 baseline** arm
rather than arriving as a migration — the same reasoning as ADR-0003, inside
the window that ADR closes at the first tagged release.

## Consequences

Easier: a full home-directory scan persists as tens of megabytes, not
gigabytes; keep-last-N retention is cheap enough to default generously; the
backup posture in `docs/data-safety.md` (entries are regenerable, size-capped)
gets teeth instead of a caveat.

Harder: file listings and search (`/files`, `find_large_files`) only see
directories and files ≥ 1 MiB. For a disk-space product this is the point —
a file below 1 MiB is never individually actionable for reclaim — but it is a
**wire-visible product fact**: the OPE behavior spec (07) and the website's
persistence copy must state it, and the e2e fixture tree must include files
on both sides of the threshold so parity tests pin the boundary.

Given up: the ability to answer "list every file from last Tuesday's scan"
from the database alone. The answer for small files is a rescan — which is
also the only honest answer, since a week-old row about a 40 KB file proves
nothing about the disk today.

Escape hatch: the threshold is a constant, not a schema property. Full
persistence (or a configurable threshold) is a post-1.0 additive change —
same tables, more rows — should a real need appear.

## See also

- ADR-0003 — the baseline-reset window this decision's schema change rides in
- `docs/data-safety.md` — regenerability and backup posture
- `rust/phantom-core/src/store.rs` — the measurement test

## Addendum 2026-09-16 — directories obey the threshold too (1.1.1, phantom-cnr.10)

**What the measurement missed.** The decision above applied the 1 MiB rule
to files and kept *every* directory. Measured on the live database (newest
scan of the maintainer's home directory: 247.7 GB, 3.50 M files, 550,137 directories):

| Newest home scan | Value |
|---|---|
| Rows persisted | 574,503 |
| … file rows (≥ 1 MiB) | 24,366 |
| … **directory rows (every directory)** | **550,137 (96 %)** |
| Directory rows whose whole subtree is < 1 MiB | 520,176 (94.5 %) |
| Empty directories persisted | 42,809 |
| Bytes per row, all-in (row ~343 B + two indexes ~315 B) | ~700 B |
| Per-scan footprint | **~420 MB**, not the "tens of megabytes" predicted |
| Directory rows if they obey the same 1 MiB rule | 54,327 |
| Per-scan footprint after the rule | **~40 MB** (~10× fewer rows) |

The half-million small directories are `node_modules`, `.git`, tool and
package caches — skeletons nothing drills into because nothing in them is
individually actionable, which is this ADR's own argument for dropping
small files.

**Amended decision.** A directory row persists iff its fully aggregated
subtree `diskSize` (or, where larger, the allocation it touches —
`privateSize + sharedSize`, so a Finder-duplicated tree whose du-model size
is 0 keeps its "deleting this frees nothing" verdict) is **≥ 1 MiB** — the
same inclusive constant as files — **or** it is pinned: the scan root, every
hotspot `topPath` in the scan's summary (plan creation and verify read
private bytes by path and must never meet a missing row), and every mount
point (a directory the walk did not descend has 0 bytes by construction;
its row *is* the boundary, ADR-0006). Pinned rows bring their ancestor
chain. The size rule is monotone on its own (a parent's aggregate includes
every child's), so the persisted rows always form a tree connected from the
root.

**Consequences.**

- Schema-free: `CURRENT_VERSION` stays 7; a 1.1.0 binary reads the file and
  simply sees fewer rows.
- A path with no row is *not* a path that does not exist. `GET
  /scans/{id}/entry`, `/tree`, `/treemap?root=` and `/explain` answer 404
  with a body that says the path is **not individually persisted** (absent,
  or a file or subtree under 1 MiB) and names the nearest persisted
  ancestor — the row holding its bytes and counts.
- A kept directory can have `dirCount > 0` and no persisted children (a
  cache of 300 tiny files in two subfolders): `/tree` on it is honestly `[]`
  and the row's counts are what a client renders; the treemap's residual
  tile carries its size.
- Diff: `before: null` / `after: null` means "no row in that scan" — the
  directory was absent **or** below 1 MiB there; the delta is right to
  within `DIFF_MIN_DELTA` (already 1 MiB).
- Growth `topLevelDir` series: a top-level directory under 1 MiB is folded
  into `other`.
- Verify: plan paths are always topPaths, so the "unmeasured" branch is
  unreachable for them; it stays for corrupted or hand-edited plans.
- Fixtures: test trees put a ≥ 1 MiB file wherever a directory must stay
  visible, at depth ≥ 2 so nesting is exercised, and every tree parity check
  asserts a non-empty exact set — a gate that passes "empty == empty" covers
  nothing. The threshold is never lowered for tests.

Pinned by `rust/phantom-core/src/persist.rs` (the home-shaped fixture, the
inclusive boundary, the topPath pin, the mount-point pin, "every kept row's
parent is kept"), `rust/phantom-api/tests/test_scans.rs`
(`folded_directories_are_explained_and_kept_rows_show_their_counts`),
`test_plans.rs` (`plan_creation_succeeds_when_a_hotspot_root_is_under_one_mebibyte`),
`test_insight.rs` (explain on a folded directory) and `tests/e2e/run-e2e.sh`
section 8.
