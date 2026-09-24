# ADR-0006: Three sizes per entry, clone groups charged once, one filesystem by default

**Status:** Accepted (2026-09-07, v1.1 Phase 1 — phantom-mkn.1, mkn.2, phantom-jsz)

## Context

v1.0's headline number lied under APFS clones. `cp -c`, Finder Duplicate,
and some checkouts produce files whose `st_blocks` reports the full
allocation on EVERY copy while the container holds one set of blocks. A
Finder-duplicated project read as fully "reclaimable" when deleting the copy
freed ~nothing. Hardlinks had already been fixed (phantom-5ws, the
`(dev, ino)` rule); clones are the same lie with no `nlink` to reveal it.

Reading the clone attributes needs `getattrlistbulk(2)` (`ATTR_CMNEXT_CLONEID`,
`ATTR_CMNEXT_PRIVATESIZE`, `ATTR_CMNEXT_EXT_FLAGS`, `ATTR_CMNEXT_CLONE_REFCNT`)
— one syscall per directory batch instead of one `lstat` per entry, which is
also the perf model the faster peers (dua-cli, OpenDisk) moved to. The spike
(walk-bench, epic notes on phantom-mkn.20) found: bulk is 3–6× faster on
the fixture tree and a 50k-file flat directory; SERIAL bulk was 2.7× SLOWER
than the parallel jwalk walk on ~/Code (156k directories — per-directory
syscall latency dominates), so the shipped walker lists directories in
parallel and assembles the result in a deterministic order.

Two facts the spike established that shape everything below:

- Pure clones share a `CLONEID` and report `PRIVATESIZE` 0 with
  `CLONE_REFCNT` ≥ 2. A clone that was WRITTEN TO gets its OWN clone id,
  refcnt 1, and a `PRIVATESIZE` equal to exactly its rewritten blocks — it
  still shares most of its extents with its origin, but nothing per-file
  says with whom.
- `st_dev` is UNIFIED across an APFS volume group: `/`, `/Users` (a
  firmlink) and `/System/Volumes/Data` (the data volume's mount point) all
  report the same device. A "do not cross `st_dev`" rule — `du -x` —
  cannot see the system↔data boundary at all.

## Decision

**Three sizes on every entry and every hotspot group**, answering three
different questions:

| Field | Question | Model |
|---|---|---|
| `diskSize` | how much is allocated? | du: `st_blocks × 512`; every sharing group (hardlinked inode OR pure-clone stream) charged ONCE per scan to its first reference in walk order. Stays THE size. |
| `privateSize` | what would deleting THIS free right now? | the kernel's `PRIVATESIZE` for a file (0 for a pure clone, 0 when a snapshot holds it, the rewritten blocks for a modified clone), forced to 0 while other hard links exist; for a directory, the rollup where a sharing group counts only if EVERY reference to it lies inside the subtree AND inside the scan (`ShareLedger`) |
| `sharedSize` | what does something else pin? | `diskSize − privateSize` for a file; for a directory the allocation of every group also referenced outside it plus the shared part of partially-cloned files |

**A clone group is a `ChargeKey::Clone { dev, clone_id }`**, charged exactly
like `ChargeKey::Inode`. Membership = `CLONE_REFCNT > 1`. Every aggregator
(`LinkCharger`, `persistable_entries`, `classify`, `totals_by_file_type`)
keys on `ChargeKey::of_entry`, so there is one dedup rule, not four.

**Reclaim estimates are Σ `privateSize`**, never Σ `diskSize`:
`HotspotsSummary.reclaimEstimate` and `HotspotGroup.privateSize` settle each
sharing group against the reference count seen inside the set being deleted.

**One filesystem by default; firmlinks followed; mount points are the
boundary.** `crossVolumes` (request field, `--cross-volumes`, MCP
`crossVolumes`) defaults to false. The boundary signal is
`DIR_MNTSTATUS_MNTPOINT` on the directory entry, not `st_dev`. A mount point
is recorded as a row flagged `mountPoint` (visible, not walked). Firmlinks
(`SF_FIRMLINK`) are descended: they are the OS's own unified view, and the
data volume's mount point is what would double-count it — that is a mount
point, so scanning `/` counts every byte exactly once.

**Compressed files own their blocks.** decmpfs (`UF_COMPRESSED`) keeps the
data in the resource fork; the kernel reports `PRIVATESIZE` 0 and
`CLONE_REFCNT` 0 for what is an empty data fork. Such a file's
`privateSize` is its allocation and it carries the `compressed` flag.

**Wire and schema.** Schema v5 adds `entries.private_size / shared_size /
clone_id / flags` and `scans.total_private_size / total_shared_size`, all
nullable: NULL == not recorded (pre-v5 rows), never backfilled — assuming
`private == disk` would restate the lie. `flags` is a bitmask in SQLite and a
sorted string array on the wire; decoders ignore unknown strings. OPE
`VERSION` 1.0.0 → 1.1.0 (additive: Minor).

## Alternatives considered

- **Firmlinks as boundaries (the plan's first wording).** Consistent with
  `du -x` on paper, but `st_dev` cannot implement it and, implemented via
  `SF_FIRMLINK`, it makes a scan of `/` show the 12 GB sealed system volume
  and nothing else — the users' data vanishes from the view Finder shows
  them. Rejected; the mount-point rule gives the same no-double-count
  guarantee with the expected result.
- **Backfilling `private_size = disk_size` on migration.** Rejected: it
  states as a fact something v1.1 exists to stop assuming.
- **Charging partial clones at `privateSize` in `diskSize`.** Exact for one
  modified clone of a surviving original, badly wrong for two modified
  clones of a deleted original. `diskSize` keeps the du model; the truth
  rides `privateSize`.
- **A per-file `getattrlist` fallback (the plan's G1 fallback).** Not
  needed: the FFI worked; the perf gate was met by parallelizing.

## Consequences

- The headline number is exact under pure clones; hotspot promises are
  what deletion returns. A Finder-duplicated project's `node_modules` now
  reads "1 MiB, deleting frees 4 KiB".
- Live `bytesSeen` dedupes through a shared set during the parallel walk,
  so it converges on `totalDiskSize` (equal at completion); WHICH member of
  a group was counted live is timing-dependent and irrelevant.
- Known limits, documented in `wire-format.md` "Three sizes": a pure-clone
  group is assumed to own its blocks (over-estimate when a modified sibling
  exists elsewhere); a modified clone's shared portion always reads as
  shared, even when its origin is in the same directory.
- Phase 2 gets `dataless` / `compressed` / `purgeable` flags for free: the
  cloud-dataloaded heuristic (logical ≫ disk) can now be told apart from
  transparent compression.
- `jwalk` is a dev-dependency only (the benchmark example's legacy walker).
