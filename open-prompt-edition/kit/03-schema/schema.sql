-- Phantom schema, v7 (current). The live source of truth is
-- rust/phantom-core/src/schema.rs; this file mirrors it for rebuilds and
-- MUST change in the same push (the OPE version gate watches both, and the
-- conformance harness diffs this file against the reference database's
-- actual sqlite_master when it self-boots — drift fails a check, not a
-- review).
--
-- Versioning: SQLite `user_version` pragma. Migrations are forward-only and
-- additive; an implementation MUST refuse to open a database whose
-- user_version exceeds what it supports. Each migration step and its version
-- bump MUST be one transaction (a crash mid-migration leaves the DB cleanly
-- at the previous version, re-runnable — never schema-ahead-of-version).
--
-- The ladder:
--   v1  the Phantom baseline (ADR-0003): scans + entries + scan_file_types.
--       (The template's Note slice was deleted from the baseline before the
--       first tagged release; no notes table has ever shipped.)
--   v2  ALTER TABLE scans ADD COLUMN hotspots TEXT — the per-scan
--       reclaimability summary.
--   v3  ALTER TABLE entries ADD COLUMN file_count INTEGER;
--       ALTER TABLE entries ADD COLUMN dir_count INTEGER — per-directory
--       full-walk descendant counts.
--   v4  ALTER TABLE scans ADD COLUMN unreadable_paths TEXT — capped
--       unreadable-path sample (phantom-671).
--   v5  clone awareness: entries private_size/shared_size/clone_id/flags,
--       scans total_private_size/total_shared_size (ADR-0006).
--   v6  ALTER TABLE scans ADD COLUMN failure_reason TEXT — running rows are
--       persisted at start; a cold start marks orphans 'interrupted: …'
--       (phantom-aoa).
--   v7  CREATE TABLE plans — reclaim plans, stored as wire JSON
--       (phantom-mkn.7).
--
-- Connections MUST run with foreign_keys ON (stock SQLite ships with them
-- OFF per connection; without enforcement ON DELETE CASCADE never fires and
-- deleted scans leak their entries forever). File stores run in WAL mode so
-- readers do not block during a scan's large insert batch.

-- v1: the Phantom baseline (ADR-0003).
--
-- Sizes are bytes; disk_size is st_blocks × 512 (THE size), logical_size is
-- secondary (kept for cloud-dataloaded detection, logical >> disk).
-- Datetimes are canonical wire strings (6-digit micros, Z form); UUIDs are
-- lowercase hyphenated text.

CREATE TABLE scans (
    id                 TEXT PRIMARY KEY,   -- UUID, lowercase hyphenated
    root_path          TEXT NOT NULL,
    status             TEXT NOT NULL CHECK (status IN
        ('running', 'complete', 'cancelled', 'failed')),
    started_at         TEXT NOT NULL,      -- canonical wire datetime
    finished_at        TEXT,               -- NULL until terminal
    total_disk_size    INTEGER NOT NULL,
    total_logical_size INTEGER NOT NULL,
    file_count         INTEGER NOT NULL,
    dir_count          INTEGER NOT NULL,
    error_count        INTEGER NOT NULL,   -- unreadable entries, skipped not fatal
    hotspots           TEXT,               -- v2 (see the ALTER below)
    unreadable_paths   TEXT,               -- v4 (see the ALTER below)
    total_private_size INTEGER,            -- v5: root's privateSize (see below)
    total_shared_size  INTEGER,            -- v5: root's sharedSize
    failure_reason     TEXT                -- v6: why a failed scan failed (see below)
);

-- One row per persisted filesystem entry. Per ADR-0005 a complete scan
-- persists every directory (disk/logical sizes replaced by the aggregate
-- over ALL descendant files) plus every file with disk_size >= 1 MiB
-- (1,048,576 bytes, inclusive); cancelled/failed scans persist no entries.
--
-- entries.category is the reclaimability category, stamped by the
-- classifier post-pass at scan completion; the stored string IS the wire
-- string (camelCase — the enum is pinned in 06-interchange/wire-format.md).
-- NULL == ordinary content, nothing to say about it. Directory rows under a
-- hotspot root carry the root's category too. nlink/dev/ino feed the
-- classifier's hardlink dedup (rows sharing (dev, ino) with nlink > 1 are
-- one physical file, counted once).
--
-- UNIQUE (scan_id, path) doubles as the per-scan index (a scan cannot
-- contain the same path twice, and its index serves scan_id-prefix
-- lookups); (scan_id, parent_path) serves direct-children queries.
CREATE TABLE entries (
    scan_id      TEXT NOT NULL REFERENCES scans(id)
                     ON DELETE CASCADE,
    path         TEXT NOT NULL,
    parent_path  TEXT,                     -- NULL exactly for the scan root
    name         TEXT NOT NULL,
    is_dir       INTEGER NOT NULL,
    disk_size    INTEGER NOT NULL,
    logical_size INTEGER NOT NULL,
    modified_at  TEXT,                     -- canonical wire datetime or NULL
    file_type    TEXT,                     -- lowercased extension; NULL for dirs/extensionless
    category     TEXT,                     -- reclaimability wire string or NULL
    nlink        INTEGER NOT NULL,
    dev          INTEGER NOT NULL,
    ino          INTEGER NOT NULL,
    file_count   INTEGER,                  -- v3: dir rows only (see below)
    dir_count    INTEGER,                  -- v3: dir rows only (see below)
    private_size INTEGER,                  -- v5: bytes deletion frees (see below)
    shared_size  INTEGER,                  -- v5: bytes other refs pin
    clone_id     INTEGER,                  -- v5: APFS clone group, NULL unless a pure clone
    flags        INTEGER,                  -- v5: EntryFlags bitmask (see below)
    UNIQUE (scan_id, path)
);
CREATE INDEX entries_by_scan_parent
    ON entries (scan_id, parent_path);

-- Per-scan aggregate totals by file type, computed from the FULL walk
-- before the ADR-0005 persistence filter, so type breakdowns see files
-- whose individual rows were omitted. Joins the v1 baseline (not a v2 arm)
-- inside the pre-release window ADR-0003 closes at the first tagged release.
CREATE TABLE scan_file_types (
    scan_id    TEXT NOT NULL REFERENCES scans(id)
                   ON DELETE CASCADE,
    file_type  TEXT,                       -- NULL == no extension
    disk_size  INTEGER NOT NULL,
    file_count INTEGER NOT NULL,
    UNIQUE (scan_id, file_type)
);

-- v2: the per-scan hotspots summary, stored as the camelCase wire JSON of
-- HotspotsSummary (06-interchange/wire-format.md; the DB string and the
-- wire string are the same string, like entries.category). NULL == no
-- summary: cancelled/failed scans (partial results are discarded) and rows
-- persisted before v2. The migration arm is exactly:
--
--   ALTER TABLE scans ADD COLUMN hotspots TEXT;
--
-- (The CREATE TABLE scans above already includes the column so this file
-- describes the CURRENT shape; a rebuild implementing the ladder applies
-- the ALTER as its v1 -> v2 step.)

-- v3 (phantom-chp): per-directory descendant counts, aggregated from the
-- FULL walk at persistence time (like dir disk sizes) — a client-side count
-- over persisted rows misses every sub-1-MiB file. NULL == not recorded:
-- file rows always, and dir rows persisted before v3 (the walk is gone; a
-- backfill would be the undercount). dir_count excludes the entry itself.
-- The migration arm, one transaction, is exactly:
--
--   ALTER TABLE entries ADD COLUMN file_count INTEGER;
--   ALTER TABLE entries ADD COLUMN dir_count INTEGER;
--
-- (Again inlined into CREATE TABLE entries above for the current shape.)

-- v4 (phantom-671): capped unreadable-path sample, stored as the camelCase
-- wire JSON array of UnreadablePath (06-interchange/wire-format.md; DB
-- string == wire string, like hotspots). NULL == not recorded: rows
-- persisted before v4. error_count stays the truth — this is the first 100
-- paths (walk order) with the OS's reason for each. The migration arm is
-- exactly:
--
--   ALTER TABLE scans ADD COLUMN unreadable_paths TEXT;
--
-- (Inlined into CREATE TABLE scans above for the current shape.)

-- v5 (phantom-mkn.1 / mkn.2, v1.1 Phase 1): what deletion actually frees.
-- entries.private_size: files — the kernel's APFS PRIVATESIZE (bytes not
-- shared with another file or snapshot), forced to 0 while other hard links
-- exist; dir rows — the subtree rollup where a hardlink/clone group counts
-- only if EVERY reference to it is inside the subtree (and inside the
-- scan). entries.shared_size: disk_size − private_size for files; for dirs
-- the allocation of every group also referenced outside the subtree plus
-- the shared part of partially-cloned files. entries.clone_id: the APFS
-- clone-group id (ATTR_CMNEXT_CLONEID), NULL unless the file shares ALL its
-- blocks with ≥1 other file; every member carries the same id and the group
-- is charged ONCE per scan, exactly like a hardlinked (dev, ino).
-- entries.flags: bitmask of the wire flag strings in bit order —
-- 1 mayShareBlocks, 2 sharesAllBlocks, 4 purgeable, 8 sparse, 16 dataless,
-- 32 firmlink, 64 mountPoint, 128 compressed. scans.total_private_size / total_shared_size:
-- the root directory's rollup. NULL == not recorded: rows persisted before
-- v5 (the walk is gone; assuming private == disk would restate the lie
-- v1.0 told). The migration arm, one transaction, is exactly:
--
--   ALTER TABLE entries ADD COLUMN private_size INTEGER;
--   ALTER TABLE entries ADD COLUMN shared_size INTEGER;
--   ALTER TABLE entries ADD COLUMN clone_id INTEGER;
--   ALTER TABLE entries ADD COLUMN flags INTEGER;
--   ALTER TABLE scans ADD COLUMN total_private_size INTEGER;
--   ALTER TABLE scans ADD COLUMN total_shared_size INTEGER;
--
-- (Inlined into the CREATE TABLEs above for the current shape.)
--
-- v6 (1.1.0, phantom-aoa): running scans are persisted at start (the same
-- scans row, upserted by id when the walk finishes), so a server that dies
-- mid-walk leaves a row the next cold start marks `failed` with
-- failure_reason = 'interrupted: …'. failure_reason also carries the walker's
-- error text for any other failed scan; NULL for every other status and for
-- rows persisted before v6. The migration arm is exactly:
--
--   ALTER TABLE scans ADD COLUMN failure_reason TEXT;

-- v7 (1.1.0, phantom-mkn.7): reclaim plans. The dry-run an agent confirms
-- before anything moves, stored as its own wire JSON (like scans.hotspots)
-- so a later `phantom verify <planId>` can compare the promise with a
-- rescan. scan_id is deliberately NOT a foreign key: the plan must outlive
-- its scan's keep-last-25 retention. Keep-last-25 retention of its own.
CREATE TABLE plans (
    id         TEXT PRIMARY KEY,               -- planId (UUID, lowercase)
    scan_id    TEXT NOT NULL,                  -- the scan the plan was built from
    root_path  TEXT NOT NULL,
    created_at TEXT NOT NULL,                  -- canonical wire datetime
    plan       TEXT NOT NULL                   -- the ReclaimPlan wire JSON
);
CREATE INDEX plans_created ON plans (created_at DESC);

PRAGMA user_version = 7;
