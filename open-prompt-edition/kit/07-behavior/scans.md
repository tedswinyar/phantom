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

Scenario: The scan collection is bounded by per-root retention (1.1.0)
  Given 25 persisted scans of root R and one older scan of another root
  When a 26th scan of R reaches any terminal state and persists
  Then GET /scans lists exactly 26 scans: R's newest 25 and the other root's one
  And R's scan with the OLDEST startedAt is gone, its entries and type
      totals cascaded with it — the other root's older scan survives
  And later reads of the evicted id are clean 404s
  And a total cap (100 by default) prunes the globally oldest once every
      root is within its own cap
  # A prune failure never fails the scan itself. Pinned by test_scans.rs
  # (completion_prunes_to_the_newest_keep_last_scans).

Scenario: The scan collection is bounded by a byte budget, never below a root's last two (1.1.1)
  Given three persisted scans of root A, each holding entry rows, and a
        byte budget (PHANTOM_DB_BUDGET_BYTES, 2 GiB by default) that the
        history exceeds
  When a scan of root R reaches a terminal state and persists
  Then A's scan with the OLDEST startedAt is gone, its entries and type
      totals cascaded with it — the eviction is by bytes, not by count
  And A's two newer scans survive even though the history is still over
      budget — the floor of two completed scans per root is never crossed
      while the root's newest completed scan is 30 days old or younger
  And R's scan survives — its root holds one
  And later reads of the evicted id are clean 404s
  # The API log states the shortfall when the floor alone exceeds the
  # budget. A scan is charged for the rows it stored, not its fileCount.
  # Pinned by test_scans.rs
  # (completion_evicts_the_oldest_scan_above_the_floor_when_over_budget)
  # and store.rs (a_roots_last_two_scans_survive_a_tiny_budget,
  # a_scan_of_many_small_files_costs_what_its_rows_cost).

Scenario: A root nobody has scanned in 30 days loses its floor (1.1.1)
  Given two persisted scans of root S whose newest startedAt is 45 days
        ago, two of root F whose newest is 3 days ago, and a byte budget
        the history exceeds
  When a scan of root R reaches a terminal state and persists
  Then both of S's scans are gone, oldest first — S's floor is 0 because
      its newest completed scan is older than 30 days
  And F's two scans survive — its newest is within 30 days, so its floor
      of two holds
  And the API log carries one "retention: floor waived" line naming root S
      and its age in days
  And a root whose newest scan is exactly 30 days old is still protected;
      one second later it is not
  # A stale root is left alone while the history is under budget: the
  # waiver only ever decides an eviction the budget needed. Pinned by
  # test_scans.rs (completion_evicts_a_stale_roots_history_and_logs_the_waived_floor)
  # and store.rs (a_stale_roots_pair_is_evicted_while_a_fresh_roots_pair_survives,
  # the_floor_holds_at_exactly_thirty_days_and_lapses_a_second_later).

Scenario: Directory rows obey the 1 MiB rule, and a folded path says where its bytes went (1.1.1)
  Given a root holding cache/a and cache/b with 150 files of 4 KiB each,
        sub/deep/nested.blob of just over 1 MiB, and an empty directory
  When the scan completes
  Then GET /scans/{id}/tree lists exactly cache and sub — a/, b/ and empty/
       have no rows, yet the scan's dirCount counts all six directories
  And GET /scans/{id}/tree?path=<root>/cache is [] while
       GET /scans/{id}/entry?path=<root>/cache reports fileCount 300 and
       dirCount 2 — the counts are the truth, the empty listing is honest
  And GET /scans/{id}/tree?path=<root>/sub lists deep, and
       ?path=<root>/sub/deep lists nested.blob — nesting is reached level
       by level, never flattened away
  And GET /scans/{id}/entry?path=<root>/cache/a is 404 whose error says the
       path is "not individually persisted" and names <root>/cache as the
       nearest persisted ancestor; /tree, /treemap?root= and /explain answer
       a folded path the same way
  # Pinned by test_scans.rs (folded_directories_are_explained_and_kept_rows
  # _show_their_counts), test_insight.rs (explain on a folded directory),
  # persist.rs unit tests, and tests/e2e/run-e2e.sh section 8.

Scenario: A hotspot root keeps its row even under 1 MiB, so a plan can be built from it (1.1.1)
  Given a Cargo project whose target/ holds one 100 KiB file
  When the scan completes and POST /scans/{id}/plan is sent
  Then the response is 201 with the plan's single item naming target/ and
       an expectedFreedBytes below 1 MiB — never a 404 for a missing row
  And GET /scans/{id}/entry?path=<proj>/target is 200 while
       ?path=<proj>/src is 404: only the hotspot topPath (and its
       ancestors) is pinned
  And after target/ is removed and the root rescanned, POST
       /plans/{planId}/verify reports the item with beforeBytes present and
       afterBytes 0 — measured, not unmeasured
  # Pinned by test_plans.rs
  # (plan_creation_succeeds_when_a_hotspot_root_is_under_one_mebibyte).

Scenario: Every scan is attributable in the API log (1.1.1)
  Given a client that sends User-Agent "phantom-test/9.9 (+probe)"
  When it POSTs /scans for <root> with crossVolumes true and olderThan "3M"
  Then the API log gains exactly one line "scan requested" carrying the
       scan id, root=<root>, cross_volumes=true, older_than=3M,
       verify_locks=false, tool_estimates=false and
       client=phantom-test/9.9 (+probe)
  And on completion one line "scan finished" with the status, duration,
       files, dirs, bytes, rows persisted and the scan's estimated bytes
       in the database
  And one "retention: pruned N scans; estimated X -> Y of a Z budget" line
       whether or not anything was pruned
  And a request with no User-Agent logs client=- rather than nothing
  # Pinned by test_scans.rs (every_post_scans_leaves_an_attributable_log_line)
  # and tests/e2e/run-e2e.sh section 17 (the CLI, the MCP server and
  # verify's implicit rescan name themselves: phantom-cli/<v>,
  # phantom-mcp/<v> (<host>), "(+verify)").

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
