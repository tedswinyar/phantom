# Changelog

All notable changes to Phantom.

## [1.0.0] - 2026-09-03

### Added

- Sparkle 2 auto-update — signed appcast on the public repo's Releases (phantom-pxt)
- Scan diff — what grew, what was freed between two scans (phantom-081)
- Capped unreadable-path sample on every scan (phantom-671)
- API-freeze fixes — command field, strict queries, encoding, 1.x policy (phantom-ojj)
- CI — verify.sh becomes an enforced gate, not an advisory one (phantom-ojj)
- Full Disk Access detect-and-guide onboarding (phantom-ojj)
- Render residual pseudo-tiles — hover-labeled, click selects the folder (phantom-fzs)
- Residual 'smaller files' pseudo-tiles in the treemap layout (phantom-fzs smoke feedback)
- Legend palette, Items column, and the Mac affordances (phantom-chp)
- Folders tree — the WinDirStat-inspired outline, done as a Mac app (phantom-chp)
- Per-dir file/dir counts on the wire, aggregated from the full walk (phantom-chp)
- Treemap labels are hover-only (phantom-fzs)
- Reclaimable view — Restless Spirits pane (phantom-ntd)
- Delete the Note template slice — the scan domain stands alone (phantom-zpq)
- Scan app UI — Phase 4 unit 2 views port (phantom-fzs)
- Swift scan layer + ghost theme — Phase 4 unit 1 (phantom-fzs)
- Reclaimable surface pass — persist categories + hotspots on every surface (phantom-ntd)
- Reclaimable classifier core in phantom-core (phantom-ntd)
- CLI + MCP scan parity and the e2e byte-parity harness (phantom-s6f)
- Async scan lifecycle in phantom-api (phantom-66s)
- Ghost app icon, generated from SVG and wired into the app bundle
- Phantom v1.0 positioning and site content (phantom-4l9)
- Scan domain in phantom-core — scanner, treemap, store, v1 schema (phantom-jik)

### Fixed

- Verify-after-sign in build-app.sh; strip toolchain rpath; exact origin match (phantom-3dr round 3)
- Pre-flip review round 2 — binary path leak, build-repo pin, loaded beads remote (phantom-3dr, phantom-pxt)
- Restless Spirits copy is paste-and-run, not a bare command (phantom-an9, phantom-fzs smoke)
- DrillOut crash on rapid Escape — removeLast on empty stack across await (phantom-fzs.1)
- Bidirectional tree<->treemap sync (phantom-7zi, phantom-fzs smoke)
- Adversarial-review must-fixes — diff sign-safety, root aliasing, test gaps (phantom-mle)
- Decimal SI sizes — the CLI, the app, and Finder say one number (phantom-2gw)
- Count a hardlinked inode once per scan — the du model (phantom-5ws)
- Safety-review must-fixes — retention wired, keyfile race, overflow, pins (phantom-ojj)
- Release pipeline rehearsed end-to-end — three Friday-blockers fixed (phantom-ojj)
- Starting another analysis is discoverable (phantom-fzs smoke feedback)
- Treemap labels place by collision, not heuristics (phantom-fzs)
- Treemap legibility — Ted's smoke found it unusable (phantom-fzs)
- Bundled CLI clobbered the app executable on case-insensitive APFS
- OPE gate bootstrap on a repository's first push

### Documentation

- Prompts + structure pass — the kit now honors its own no-source promise (phantom-ojj)
- Safety-review decisions — arm64-only wording, fresh-Mac hard gate, view-test decision (phantom-ojj)
- Hero shows the legend-colored map, Items column, live tooltip
- Folders tree + legend design spec (phantom-chp)
- Kit fully filled — every section reflects main@a388242 (phantom-jqk, phantom-nk1)
- Content pass — every claim verified against main@a388242 (phantom-3hu)
- Draft contract sections 02/03/04/06 from the implemented API
- Two earned gotchas — bd dolt push GH007 identity, MutexGuard match-scrutinee deadlock
- ADR-0005 — persist directories and large files, not the full walk
- ADR-0004 — system-pressure monitoring belongs to Banshee, not Phantom
- ADR-0003 — reset schema baseline to a single Phantom v1 (phantom-jik)
- Earned gotchas from Phantom v0.1 and the disk-cleanup playbook

### Changed

- Treemap selection stroke 3pt -> 4pt (Ted: slightly more visible)

### Other

- Bump every version source to 1.0.0; promote website claims to shipped

