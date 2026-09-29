# Fixture projects — one per classifier project type

Each directory holds exactly a DETECTION file, one ARTIFACT directory with a
small file in it, and (where the type has one) a LOCKFILE. `decoys/` holds
the same artifact directory names with NO detection file beside them.

Two suites walk this tree with the real scanner and assert the EXACT set of
groups (Gate G2 of v1.1 Phase 2: zero false positives, nothing from
`decoys/`):

- `rust/phantom-core/src/classify.rs` (`fixture_projects_classify_exactly`)
- `tests/e2e/run-e2e.sh` (section 11d)

Mutation: delete a detection file here and both fail — the artifact beside
it leaves the reclaimable set. mtimes are whatever checkout gave them, so
nothing here depends on staleness.
