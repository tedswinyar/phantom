# 07 — Behavior

Given/When/Then scenarios describing observable behavior, implementation-
agnostic. The scan lifecycle is the heart of the product; its state machine
is:

```
                    POST /scans (202)
                          │
                          ▼
                      running ──── cancel requested / walk error
                          │                    │
                     walk finishes             │
                          │                    │
                          ▼                    ▼
                      complete        cancelled | failed
```

`running` → exactly one of `complete` | `cancelled` | `failed`. Terminal is
terminal; there are no other transitions. Every terminal state persists a
scan row; ONLY `complete` persists results (entries, type totals, hotspots).

```gherkin
Scenario: Starting a scan answers before the walk does
  Given the API is running on a test profile
  When POST /scans is sent with a valid directory rootPath
  Then the response is 202 with status "running", finishedAt null,
       and a progress OBJECT with zeroed counters
  And polling GET /scans/{id} eventually shows a terminal status,
       finishedAt set to a canonical datetime, and progress null
  # Pinned by rust/phantom-api/tests/test_scans.rs (scan_lifecycle_end_to_end)
  # and 09-conformance (create/poll section).

Scenario: A completed scan reads back identically through every client
  Given a completed scan of a deterministic fixture tree
  When the scan, its files, its types, and its hotspots are fetched via
       raw HTTP, via the CLI (--json), and via the MCP tools
  Then each surface's JSON documents are byte-identical after key-sorting
  # Pinned by tests/e2e/run-e2e.sh — this scenario IS the parity gate.

Scenario: Results are gated until the scan finishes
  Given a scan that is still running
  When any results endpoint (treemap, tree, files, entry, types, hotspots)
       is fetched
  Then the API answers 409 with an error pointing back at polling,
       never 404 and never an empty result
  # Pinned by test_scans.rs (types_route_error_branches,
  # hotspots_error_branches, cancel_mid_scan…).

Scenario: Cancellation is cooperative and discards partial results
  Given a scan that is still running
  When POST /scans/{id}/cancel is sent
  Then the response is 202 and the scan later lands status "cancelled"
  And its metadata row records the attempt with zero totals
  And every results surface serves the honest empty answer
      (files/tree: [], treemap: no rects, hotspots: the empty summary)
  When cancel is sent again
  Then the API answers 409 naming the terminal status
  # Cancel-of-terminal is a conflict, not a no-op: a blind retry must find
  # out. Pinned by test_scans.rs (cancel_mid_scan_is_deterministic…).

Scenario: A scan is never invisible, even when persistence fails
  Given a scan whose terminal persist fails (e.g. the insert is rejected)
  When GET /scans/{id} is fetched
  Then the scan is still visible, with status "failed"
  And DELETE /scans/{id} forgets it cleanly (204)
  # Persist-then-remove ordering; pinned by test_scans.rs
  # (handoff_failure_keeps_the_scan_visible).

Scenario: Deleting a running scan is refused
  Given a scan that is still running
  When DELETE /scans/{id} is sent
  Then the API answers 409 telling the caller to cancel first
  # Deleting mid-walk would race the completion handoff. Pinned by
  # test_scans.rs (delete_scan_cascades_and_running_scans_refuse).

Scenario: Classification happens once, at completion, over the full walk
  Given a directory containing node_modules with one 1 MiB file and one
        10-byte file
  When the scan completes and GET /scans/{id}/hotspots is fetched
  Then the node-modules group's fileCount is 2 and the reclaimEstimate
       exceeds 1 MiB — the small file counts even though it has no
       persisted entry row
  And the node_modules DIRECTORY row and the big file's row both carry
       category "regenerableArtifact", while unrelated entries carry
       category null (present, not absent)
  # Pinned by test_scans.rs (hotspots_classify_on_completion…) and
  # tests/e2e/run-e2e.sh section 14.

Scenario: The scan collection is bounded by keep-last-25 retention
  Given 25 persisted scans
  When a 26th scan reaches any terminal state and persists
  Then GET /scans lists exactly 25 scans
  And the scan with the OLDEST startedAt is gone, its entries and type
      totals cascaded with it
  And later reads of the evicted id are clean 404s
  # A prune failure never fails the scan itself. Pinned by test_scans.rs
  # (completion_prunes_to_the_newest_keep_last_scans).

Scenario: Test profile cannot touch production data
  Given PHANTOM_PROFILE=test
  And PHANTOM_DB_PATH pointing inside the prod data directory
  When the API starts
  Then it refuses to start with a config error naming the path
  # Pinned by rust/phantom-api/src/config.rs tests.
```

## Writing new scenarios

- One observable behavior per scenario; no implementation nouns (no "the
  Rust store", no "SwiftUI view").
- Every scenario cites the test that pins it, or gets one in the same push.
