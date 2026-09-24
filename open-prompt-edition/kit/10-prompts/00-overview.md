# Prompt 00 — Overview: rebuilding Phantom

You are rebuilding Phantom from its Open Prompt Edition. Work through the
numbered prompts in order; each ends with a verifiable exit criterion.

## What you are building

A local-first macOS tool with a constellation shape: one API server owns a
SQLite database; a CLI, an MCP server, and a GUI app are thin HTTP clients
of it. The kit's contract sections (02, 03, 04, 06 — plus 05's hotspot
registry table and orderings, which are wire-visible through the results
surfaces) are authoritative; when a prompt and a contract disagree, the
contract wins. The scan lifecycle's observable behavior is scenario-form in
07; the GUI app is out of scope for a wire-compatible rebuild.

## Ground rules

1. **Conformance is the definition of done** — `09-conformance/run.sh`
   pointed at your server, plus decoding the shared fixtures
   (`08-fixtures/`) from raw bytes.
2. **The wire format is not negotiable** (`06-interchange/wire-format.md`).
   Your language's "standard" JSON/date handling is probably wrong in at
   least one of the Five Rules' ways; check before trusting it.
3. **One writer.** Only the API process opens the database.
4. **Test mode wins.** Implement the profile invariants
   (`04-config/config-spec.md`) before implementing features; they are
   safety properties.
5. **Every error branch is reached by a test.** The reference testing
   standard (mutation-proof, mock-only-at-boundaries, raw-bytes parsers)
   applies to rebuilds.

## Prompt sequence

| # | Builds | Exit criterion |
|---|---|---|
| 01 | Project scaffold + config resolution | profile tests pass, incl. refusals |
| 02 | Schema + store + migrations | migration ladder tests pass (v1 rows survive v2) |
| 03 | API server + auth + health | conformance auth/health + error-shape checks green |
| 04 | The scan domain (lifecycle, results, classifier, treemap) | conformance fully green |
| 05 | CLI | parity: CLI vs curl byte-identical |
| 06 | MCP server | parity: MCP vs curl byte-identical; tool set matches the pinned seven |
