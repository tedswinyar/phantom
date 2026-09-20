# Phantom — Open Prompt Edition (OPE)

The project published as a rebuildable kit: contracts, schema, config,
wire format, fixtures, behavior specs, conformance tests, and ordered build
prompts. An independent implementation (any language) built from this kit
should pass the conformance harness against the same database as the
reference implementation.

**Version**: see `VERSION`. It moves with the app version (`VERSIONING.md`)
and MUST be bumped in any push that changes a contract surface — enforced by
`scripts/check-ope-version-bump.sh` in the pre-push gate.

## Rebuilding from this kit

Start at `kit/10-prompts/00-overview.md` and work the prompts in order
(01 scaffold → 02 schema/store → 03 server shell → 04 scan domain →
05 CLI → 06 MCP); each ends with a runnable exit criterion. Before writing
code, read `kit/01-audit/` (the questions, with this system's answers) and
`kit/06-interchange/wire-format.md` (the Five Rules — your language's
defaults violate at least one). You are DONE when:

1. `kit/09-conformance/run.sh <your-base-url> <your-api-key>` reports
   `0 failed` against your server;
2. your unit suite decodes every fixture in `tests/fixtures/` from raw
   bytes (`kit/08-fixtures/`);
3. the `kit/11-validate/` checklist passes, including the two-implementations
   drill against the reference.

## Kit layout

| Section | Contents | State |
|---|---|---|
| `kit/01-audit/` | Questions a rebuild must answer before writing code | **complete** |
| `kit/02-contracts/` | HTTP API + MCP protocol, endpoint by endpoint | **complete** |
| `kit/03-schema/` | The SQLite schema (schema.sql) + versioning rules | **complete** (drift-checked by conformance) |
| `kit/04-config/` | Env vars, profiles, ports, key file | **complete** |
| `kit/05-algorithms/` | Treemap + reclaimability classifier + orderings | **complete** |
| `kit/06-interchange/` | **The wire format** — exact bytes, the Five Rules | **complete** |
| `kit/07-behavior/` | Given/When/Then scenarios (the scan state machine) | **complete** |
| `kit/08-fixtures/` | Pointer to the shared fixtures (`tests/fixtures/`) | **complete** |
| `kit/09-conformance/` | Black-box harness any implementation must pass (46 checks) | **complete** |
| `kit/10-prompts/` | Ordered build prompts for rebuilding from scratch | **complete** (00–06) |
| `kit/11-validate/` | Cross-implementation validation checklist | **complete** |

**State legend:**
- **complete**: Fully written, tested, reflects the working codebase
- **seeded**: Working example present + guidance for growth; extend as the domain grows
- **template**: Placeholder structure for stamped projects to fill in

## Growth contract

Contract sections (02, 03, 06, 08 — plus 03's schema.sql, which the
conformance harness structurally diffs against the reference database) are
kept TRUE continuously — the fixtures they cite are the same files the Rust
tests, Swift tests, and conformance harness execute, so drift fails the
gate rather than accumulating. Prose sections (01, 05, 07, 10, 11) grow as
the project does.

When a contract surface changes, update the kit and bump `VERSION` in the
same push; the gate will insist anyway.
