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
| `entry.json` | a file entry: diskSize vs logicalSize divergence, millisecond datetime variant |
| `entry-dir.json` | a scan-root directory entry: every nullable field null-and-present |
| `treemap.json` | a `TreemapLayout` with nested rects — camelCase at every depth |
| `types.json` | a `FileTypeTotal` array: shape AND contract order (a size tie broken by name; the null-type bucket) |
| `hotspots-summary.json` | a `HotspotsSummary`: deduped vs listed sizes, group ordering, camelCase at every depth |

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
