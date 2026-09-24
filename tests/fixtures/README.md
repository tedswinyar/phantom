# Shared wire-format fixtures

The exact bytes of the wire format, consumed by THREE test suites so the
format cannot drift between implementations:

- Rust: `rust/phantom-core/src/scan.rs` (`include_str!`)
- Swift: `swift/Tests/PhantomCoreTests` (loaded relative to `#filePath`)
- OPE conformance: `open-prompt-edition/kit/09-conformance/run.sh`

The scan-domain set:

| Fixture | Pins |
|---|---|
| `scan-running.json` | the wire VIEW of an in-flight scan: `Scan` fields + `progress` object; a datetime without fractional digits (decode generously) |
| `scan-complete.json` | a terminal scan: `finishedAt` set, `progress` present-as-null |
| `scan-interrupted.json` | (1.1.0) a scan the server was running when it stopped: `failed` with `failureReason` `interrupted: …`, zero totals, `progress` null |
| `entry.json` | a file entry: diskSize vs logicalSize divergence, millisecond datetime variant |
| `entry-dir.json` | a scan-root directory entry: every nullable field null-and-present |
| `treemap.json` | a `TreemapLayout` with nested rects — camelCase at every depth |
| `types.json` | a `FileTypeTotal` array: shape AND contract order (a size tie broken by name; the null-type bucket) |
| `hotspots-summary.json` | a `HotspotsSummary`: deduped vs listed sizes, group ordering, camelCase at every depth; v1.1 `riskTier`/`why`/`rebuildCost`/`toolEstimate` (null and populated); Phase 3 `projects` (dormant, active, and an unverifiable root with `lastActivityDays: null`) |
| `reclaim-plan.json` | (1.1.0) a `ReclaimPlan` from `POST /scans/{id}/plan`: one safe item, the skipped counts, camelCase at every depth |
| `reclaim-verification.json` | (1.1.0) a `ReclaimVerification` from `POST /plans/{id}/verify`: per-item before/after/actual (nullable), the root delta, `withinTolerance` |
| `path-explanation.json` | (1.1.0) a `PathExplanation` from `GET /scans/{id}/explain?path=`: the three sizes, sharing facts, the pared hotspot, the unreadable sample below, the one-sentence verdict |
| `stale-projects.json` | (1.1.0) a `StaleProjects` from `GET /scans/{id}/stale?olderThan=`: re-thresholded per-project activity with artifacts |
| `volume-status.json` | (1.1.0) a `VolumeStatus` from `GET /volume?snapshots=true&scanId=`: container statfs numbers, this volume's own usage, purgeable = important − available, the snapshot list, and the `hidden` split (scanned/unscanned/other volumes/other users' homes/unreadable/the tmutil suggestion) — the arithmetic between the fields is asserted by `volume.rs` |
| `growth.json` | (1.1.0) a `Growth` from `GET /scans/series?root=&groupBy=topLevelDir`: three points oldest first (the first pre-v5 with `totalPrivateSize: null`), ranked series plus `other`, and a linear forecast whose numbers `growth.rs` re-derives from the points (exactly 10 GB/day → 60 days to full) |
| `probes/tmutil-listlocalsnapshots.txt` | (1.1.0) `tmutil listlocalsnapshots` stdout: header line + one snapshot name per line; the parser skips anything else |
| `projects/` | one fixture project per classifier project type + `decoys/`: the exact-set gate (G2), walked by the real scanner from `classify.rs` and `run-e2e.sh` (see `projects/README.md`) |
| `probes/` | raw stdout of the opt-in dry-run probes; `probe.rs` parsers start from these bytes |

These live here (not in `open-prompt-edition/`) because the OPE layer is
prunable and the fixtures are not. The OPE kit's `08-fixtures` section
points at this directory.

Rules (see `open-prompt-edition/kit/06-interchange/wire-format.md`):
- keys are camelCase at every nesting depth
- nullable fields are present-as-null, never absent
- datetimes encode as exactly 6 fractional digits with `Z`
- UUIDs encode lowercase; decoders accept any case

When a new interop bug is found, add the exact bytes that triggered it as a
new fixture and point all three suites at it.
