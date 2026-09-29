# Glossary

Project-specific terminology and coined terms used throughout the documentation.

## Architecture terms

### Constellation
The architectural pattern where one central API hub owns all database access, and multiple thin clients (GUI app, CLI, MCP server) speak HTTP to it. Each client is trivially replaceable; the hub enforces auth and invariants in one place. See: [adr/0001-constellation-architecture.md](adr/0001-constellation-architecture.md)

### Slice (template slice)
The example domain entity (`Note`) that flows through every layer of the template as a working demonstration. Files carrying it are marked `// TEMPLATE SLICE`. When stamping a new project, you replace the slice with your real domain while keeping the shape (store, routes, CLI commands, MCP tools, tests, fixtures, wire format).

### OPE (Open Prompt Edition)
The project published as a rebuildable kit: contracts, schema, wire format, fixtures, behavior specs, conformance tests, and build prompts. An independent implementation in any language should be able to rebuild the system from the OPE kit and pass the conformance harness. See: [../open-prompt-edition/](../open-prompt-edition/)

### Sentinel (stamping target)
The strings that `init.sh` replaces when stamping a new project from the template: `phantom`, `phantom`, `Phantom`, `phantom`, `PHANTOM`. In the un-stamped template, these appear only where renaming is intended. Sentinel hygiene means keeping these strings out of prose, comments, or data where stamping shouldn't touch them.

### Detection file
The file (or directory) whose presence proves a sibling artifact directory is what its name suggests: `Cargo.toml` beside `target/`, `build.gradle` beside `build/`, `go.mod` beside `vendor/`. The classifier's project table is detection file → artifact dirs; an artifact with no detection file beside it is ordinary content. `tests/fixtures/projects/` holds one project per row plus decoys.

### Risk tier (`riskTier`)
The stated deletion risk of a hotspot group — `safe`, `caution`, `review` — derived from the registry row, the category and the lock state, never asserted by a rule. `safe` needs the lockfile present (or a kind with no lockfile concept); `caution` covers missing/failed lockfiles, tool-managed caches and model caches; `review` is never a suggestion. Comes with a one-sentence `why` and a `rebuildCost`. See: [reclaimability.md](reclaimability.md)

### Carve-out
A registry row that keeps its own hotspot group even when nested inside another hotspot root (model caches and `~/.cache/uv` inside `~/.cache`). The enclosing group's totals exclude it, so every byte is still counted exactly once; without it, everything under `~/.cache` would be one undifferentiated group.

### Unverifiable (project activity)
A project root with neither a git activity file (`.git/logs/HEAD` etc.) nor a dated source file in the scan. Its age cannot be known, so it is never called dormant — missing evidence is not staleness.

## Development process terms

### Landing the plane
The session-end checklist for agents: verify.sh green, commit with a body explaining what was proven, update HANDOFF.md (local, gitignored), update the relevant bead. The term emphasizes that starting work is not the same as completing it.

### Sharpening (Sharpening Review Team)
The code review methodology where multiple specialized agents (technical reviewer, quality auditor, architect, devil's advocate) independently review, then challenge each other's findings until convergence. The Rule of Five requires multiple iterative passes before declaring "this is as good as it can be."

### The Five Rules (wire format)
The exact wire format contract enforced between Rust and Swift:
1. camelCase keys at every depth
2. Nullable fields present-as-null (never omitted)
3. Datetimes encode 6-digit-microsecond Z form (`2026-08-19T14:23:00.123456Z`), decode generously
4. UUIDs lowercase out, any case in
5. Unknown fields rejected (`deny_unknown_fields`)

See: [../open-prompt-edition/kit/06-interchange/wire-format.md](../open-prompt-edition/kit/06-interchange/wire-format.md)

### Forward-only migration
The database versioning policy where migrations only go forward in version, never backward. The schema has a `user_version` pragma; if the app finds a newer version than it knows, it refuses to open the DB rather than risk corruption. Rolling back versions requires restoring from a backup. See: [data-safety.md](data-safety.md)

### Stamp / stamping
The process of copying the spooky-shell template and running `./scripts/init.sh "My New Thing"` to rename all sentinels and create a new project. The stamp is verified (the new project's `verify.sh` runs) before `init.sh` completes, so a stamped project starts green by construction. See: [../README.md](../README.md)

### Backport
The process of porting an improvement from a stamped project back to the template, so future stamps inherit it. The backport protocol is manual and optional — fixes don't propagate automatically.

## Testing and verification terms

### Mutation-proof
A test is mutation-proof when deliberately breaking the production code (e.g., removing an `ORDER BY` clause or inverting a condition) causes that specific test to fail. If a mutation goes undetected, it usually means the test is mocking the wrong layer or not asserting the actual behavior.

### Mock at boundaries
The testing principle: drive real parsers, validators, and mappers; mock only at the network boundary (`MockAPIClient` in Swift). A test that mocks the layer it claims to verify passes while proving nothing.

### Conformance harness
The black-box test suite in `open-prompt-edition/kit/09-conformance/` that any implementation (reference or independent rebuild) must pass. It speaks HTTP to the API, uses no internal knowledge, and verifies the wire format contract. See: [../open-prompt-edition/kit/09-conformance/](../open-prompt-edition/kit/09-conformance/)

## Size terms (ADR-0006)

### diskSize
`st_blocks × 512` — bytes actually allocated on disk. THE size everywhere. Deduped: a sharing group (a hardlinked inode or an APFS pure-clone stream) is charged ONCE per scan, to its first reference in walk order — the `du` model. Diverges from `logicalSize` in both directions (sparse and compressed files: logical ≫ disk; cloud placeholders: logical ≫ disk ≈ 0).

### privateSize
What deleting THIS path would actually free, right now. For a file: the kernel's APFS `PRIVATESIZE` (0 for a pure clone or a snapshot-held file; the rewritten blocks of a modified clone), forced to 0 while other hard links exist. For a directory: the rollup where a sharing group counts only if EVERY reference to it lies inside the subtree and inside the scan. Reclaim estimates are Σ privateSize. Always ≤ diskSize.

### sharedSize
What something else pins: `diskSize − privateSize` for a file; for a directory the allocation of every group also referenced outside it, plus the shared part of partially-cloned files.

### Clone group
Files sharing an APFS `CLONEID` with `CLONE_REFCNT` ≥ 2 — pure clones of one data stream (`cp -c`, Finder Duplicate). Every member reports the full allocation to `st_blocks`; the group is charged once (`ChargeKey::Clone`, the same rule as `ChargeKey::Inode` for hardlinks). A clone that has been written to leaves the group: its own id, refcnt 1, `privateSize` = its rewritten blocks.

### ShareLedger
The per-scan record of every sharing group: allocation, references seen, and the filesystem's reference counts (`nlink`, `CLONE_REFCNT`). Answers "if I delete this set of paths, does anything still pin the blocks?" — the question `privateSize` asks and `LinkCharger` (which only counts once) cannot.

### Mount point boundary
A directory with `DIR_MNTSTATUS_MNTPOINT` set: another volume is mounted there. Recorded as a row flagged `mountPoint`, NOT walked unless the scan asked `crossVolumes`. This — not `st_dev`, which is unified across an APFS volume group — is what keeps a scan of `/` from counting the data volume twice. Firmlinks (`/Users`, `/Applications`) are followed.

### Hidden space
The gap between what a volume reports as used and what a scan counted, decomposed read-only by `GET /volume?scanId=` (v1.1 Phase 4). On APFS `statfs` describes the whole CONTAINER, so `usedBytes` includes the System, Preboot, Recovery and VM volumes; `volumeUsedBytes` (`getattrlist ATTR_VOL_SPACEUSED`) is this volume alone, and their difference is `hidden.otherVolumesBytes`. `hidden.unscannedBytes` = `volumeUsedBytes − scannedBytes`: paths outside the scan root, other users' homes (`hidden.otherUserHomes`), unreadable entries (`hidden.unreadableCount`), and blocks pinned by local Time Machine snapshots. Purgeable space (`purgeableBytes`, from CoreFoundation's important-usage capacity minus statfs available) is what Finder's "Available" silently adds. Phantom SUGGESTS `tmutil thinlocalsnapshots` (`hidden.snapshotSuggestion`) and never runs it — the arithmetic is reported, never acted on.

## Profile and configuration terms

### Profile (prod/dev/test)
The runtime mode selected by `PHANTOM_PROFILE`. Each has its own database path, port, and safety rules. Test profile refuses any DB path under the prod data directory — a mis-set env var cannot touch real data. Profiles are NOT "performance profile" or "user profile"; they're environment configurations.

### Port ladder
When the configured port is busy, the API server tries port+1, port+2, up to port+9, then gives up. The server announces the ACTUAL bound port on stdout (`phantom-api listening on http://127.0.0.1:<port>`); supervisors parse the announcement. Configured ports are requests, not facts.

## Template lifecycle terms

### Pruning (layer pruning)
Deleting entire subsystems during or after stamping. `./scripts/init.sh --no-website --no-ope --no-swift` removes those directories, and the Makefile + verify gate detect layers by presence. Pruning is pure deletion — no stub files left behind. See: [../README.md](../README.md)

### Test-init (`scripts/tests/test-init.sh`)
The gate that runs on every push to verify the template can still stamp cleanly. It creates a throwaway stamped copy, runs verify.sh on it, and checks for leftover sentinel strings. If stamping breaks, the push is blocked. This is how "the template never rots" is enforced. See: [../README.md](../README.md)
