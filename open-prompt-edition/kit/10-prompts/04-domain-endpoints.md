# Prompt 04 — The scan domain

## Task

Implement the whole scan surface per `../02-contracts/api-protocol.md`: the
async lifecycle (walker, registry, terminal handoff), the results endpoints,
the classifier post-pass, and the treemap.

## Requirements

1. POST /scans answers 202 before the walk starts; the walk runs off the
   request path, publishing live progress (filesSeen, bytesSeen as DISK
   bytes, currentPath) through an in-memory registry.
2. The walker collects, per entry: path/parent/name/isDir,
   diskSize (st_blocks × 512 — THE size), logicalSize, mtime, lowercased
   extension, nlink/dev/ino. HIDDEN files/dirs are included (dotfiles —
   the `.cache`/`.venv`/`.git`-adjacent classifier rules depend on them;
   many walkers skip them by default). Unreadable entries increment
   `errorCount` and are skipped, never fatal. Symlinks are NOT followed
   (see Sharp edges).
3. Terminal handoff, in this order: fold totals into the scan row → compute
   type totals from the FULL walk → run the classifier
   (`../05-algorithms/`) over the FULL walk and stamp `entries.category`
   (directory rows under hotspot roots included) → apply the ADR-0005
   persistence filter (every directory with full-depth size aggregates AND
   full-depth descendant `fileCount`/`dirCount` — sub-1-MiB files count in
   all of them; files ≥ 1 MiB inclusive survive as rows with null counts)
   → persist everything + the hotspots summary in one transaction → only
   then release the registry entry. If the persist fails, the scan stays
   visible in the registry as `failed`.
4. After every SUCCESSFUL terminal persist, prune the collection to the
   newest 25 scans (`../02-contracts/api-protocol.md`, GET /scans). A prune
   failure must not fail the scan — it is already persisted; log and move
   on.
5. Cancellation is cooperative (flag checked per entry) and discards
   partial results: the cancelled scan persists a metadata-only row.
6. Results endpoints (`treemap`, `tree`, `files`, `entry`, `types`,
   `hotspots`) share the gate: 409 while the scan runs, 404 unknown. Their
   parameter validation, defaults, orderings, pagination
   (X-Next-Cursor), and empty-result postures are all in the contract —
   implement every row of its tables.
7. The treemap is laid out server-side per `../05-algorithms/` (squarified;
   `root=` re-roots AND re-lays-out; the residual "smaller files"
   pseudo-tile synthesized at the 0.5% threshold — its wire shape is in
   `../06-interchange/wire-format.md`).

## Exit criterion

`../09-conformance/run.sh <your-base-url> <key>` fully green — including
the hotspots section, the 409-while-running gate, and the fixture key-set
agreements.

## Sharp edges

- A scan must NEVER be invisible: persist-then-remove, and listings dedupe
  by id for the brief both-sides window. Remove-then-persist loses scans
  when the insert fails.
- The classifier and the type totals see the FULL walk, BEFORE the
  persistence filter — a hotspot made of small files must still total
  correctly (conformance pins this via fileCount).
- Directory `diskSize` aggregates ALL descendants, including filtered ones:
  the root directory's aggregate equals the scan total, byte for byte.
- A cancel racing completion may lose; both outcomes are legal — the poll
  reveals which side won.
- If your store access is a lock, do not hold it across the
  persist-then-prune sequence as one guard (the reference implementation
  deadlocked itself re-locking inside a match on the insert result).
- Convert `st_blocks` to bytes with SATURATING multiplication; a corrupt
  stat near u64::MAX must clamp, not wrap. Do not follow symlinks: a link
  is its own ~0-block entry, loops must terminate, and a link escaping the
  scan root must not pull the target's bytes into the totals.
