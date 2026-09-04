# ADR-0003: Reset the schema baseline to a single Phantom v1 migration

- **Status**: accepted
- **Date**: 2026-08-20

## Context

Phantom was stamped from the spooky-shell template, which ships a `Note`
demonstration slice that flows through every layer. The template's
`schema.rs` carries two migration arms — v1 creates `notes`, v2 adds
`notes.closed_at` — for one reason: to prove that the forward-only migration
machinery actually works, with a test that migrates a hand-built v1 database
and shows the pre-existing row surviving.

Phase 1 of v1.0 introduces the real domain: `scans` and `entries`. That poses
a question the forward-only rule normally settles for us. Our own convention
(`docs/data-safety.md`) says migrations are "additive, one version step per
function arm, and **never rewritten once shipped**". Adding the scan tables as
a v3 arm would obey the letter of that rule, but it would encode a history
that never happened: no user has ever run a Phantom database. There are no
v1 or v2 databases in the world to migrate. The version numbers would be a
fossil record of the template's teaching example, not of this product.

The countervailing risk is that "nothing has shipped yet" is a claim that
decays silently. It is true exactly once, and the cost of being wrong about
it is a wedged database that refuses to open. So the reset needs to happen
before any release, be recorded, and not become a habit.

A second constraint shapes the baseline's contents. The Note slice is not
removed until Phase 4; until then its store and its HTTP routes still run
against this database, and the e2e and OPE conformance suites exercise them.
A baseline that dropped `notes` would take four suites red for three phases.

## Decision

We will collapse the template's v1/v2 migration steps into a **single Phantom
v1 baseline** and set `CURRENT_VERSION = 1`.

The v1 arm creates:

- `scans` — one row per scan: id (lowercase UUID text), root_path, status,
  started_at, finished_at (nullable), total_disk_size, total_logical_size,
  file_count, dir_count, error_count.
- `entries` — one row per filesystem entry, `scan_id` foreign key with
  `ON DELETE CASCADE`, indexed on `(scan_id)` and `(scan_id, parent_path)`.
- `notes` — the template slice's table, in its post-v2 shape (`closed_at`
  column present from the start), retained so the Note slice keeps working
  until Phase 4 deletes it.

The migration machinery itself — `validate_or_init`, the one-transaction
`apply_migration`, the refuse-a-newer-database check, and every one of their
tests — is kept exactly as-is. Only the contents of `migrate_step` change.
The tests that needed a two-step ladder to demonstrate a forward migration
now build a synthetic v1-with-conflicting-schema database instead, so the
rollback and recoverability guarantees stay pinned by real assertions rather
than by the template's example.

**This is a one-time reset, and it closes now.** From the first tagged
release onward, every schema change is a new arm and a `CURRENT_VERSION`
bump — the migration ladder advances one rung per schema change. There is no second
baseline reset.

## Consequences

Easier: the schema reads as the product's own history. A new contributor
opening `schema.rs` sees one arm describing the tables that exist, not two
arms about a `Note` entity that Phase 4 deletes. The v1 `entries` table is
also free to include the Phase 5 hardlink-dedup columns (`nlink`, `dev`,
`ino`) and the Phase 5 `category` column from the start, rather than arriving
as ALTER TABLE arms for columns no shipped database ever lacked.

Harder: any database created by a pre-Phase-1 build of Phantom is now
un-openable — it reports `user_version = 2` against a build that supports at
most 1, and `validate_or_init` refuses it. That refusal is the correct
behavior and is exactly what the future-database test pins. Such databases
exist only in developer working copies; the remedy is to delete the dev
database (`~/Library/Application Support/phantom/phantom-dev.db`) and rescan,
since scan data is regenerable by definition (see `docs/data-safety.md`).

Given up: the template's v1→v2 step as a live demonstration of a
multi-version ladder. The machinery is unchanged and still tested, but the
first *real* migration to v2 will be the first time this project's ladder has
more than one rung in production. The per-change touch-point discipline is the checklist
that keeps that step honest.

Escape hatch: if a Phantom database ever escapes into a user's hands before
the first tagged release — a TestFlight build, a shared dev profile — the
reset is off the table and the scan tables must land as a v3 arm instead.
The signal to watch is the first artifact published outside this repository.

## See also

- `docs/data-safety.md` — forward-only migrations, backup semantics, and the
  v1.0 posture on scan-entry regenerability
- The per-change touch-point discipline that applies from the
  first release onward
- `rust/phantom-core/src/schema.rs` — the baseline and its tests
