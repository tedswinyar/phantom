# 08 — Fixtures

The authoritative fixture bytes live at **`tests/fixtures/`** (repo root),
NOT here. Reason: this OPE layer is prunable, the fixtures are not — they
are executed by the Rust unit tests, the Swift unit tests, and the
conformance harness, which is exactly what keeps this kit true instead of
aspirational.

| Fixture | Pins |
|---|---|
| `tests/fixtures/scan-running.json` | the wire VIEW of an in-flight scan: `Scan` fields + `progress` object; a fractionless datetime (generous decode) |
| `tests/fixtures/scan-complete.json` | a terminal scan: `finishedAt` set, `progress` present-as-null |
| `tests/fixtures/entry.json` | a file entry: diskSize/logicalSize divergence, millisecond datetime variant |
| `tests/fixtures/entry-dir.json` | a scan-root directory entry: every nullable field null-and-present |
| `tests/fixtures/treemap.json` | a `TreemapLayout` with nested rects — camelCase at every depth (Rule 3) |
| `tests/fixtures/types.json` | a `FileTypeTotal` array: shape AND contract order (a size tie broken by name; the null-type bucket) |
| `tests/fixtures/hotspots-summary.json` | a `HotspotsSummary`: nested groups, deduped vs listed sizes, group ordering, the category strings |
| `tests/fixtures/datetime-variants.json` | every in-spec datetime variant of the generous-decode table, with expected canonical forms |

Adding a fixture (usually after an interop bug):

1. Capture the EXACT bytes that triggered the issue.
2. Add the file under `tests/fixtures/` and cite it in
   `06-interchange/wire-format.md`.
3. Point the Rust test (`include_str!`), Swift test (`#filePath`-relative
   load), and conformance harness at it.
4. Bump `open-prompt-edition/VERSION` — the gate insists anyway.
