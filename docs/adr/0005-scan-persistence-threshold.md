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
