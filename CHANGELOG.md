# Changelog

All notable changes to Phantom.

## [1.1.0] - 2026-09-20


**Fidelity and the agent workflow.** Phantom 1.1 tells the truth about
three things 1.0 could only approximate, and lets an agent act on it with
you holding the trigger.

**Every size is now one of three.** The walker reads APFS clone attributes
(`getattrlistbulk`), so each file and directory carries what it occupies
(`diskSize`), what deleting it would free (`privateSize`), and what another
copy pins (`sharedSize`). On the developer's own `~/Code`, 1.0 had
overstated by 3.7 GB of clones. Scans finish in less than half the time.

**Every suggestion states its risk and why.** The classifier knows twenty-six
project types by their detection files, so a directory merely named `target`
no longer counts. Each group is `safe`, `caution`, or `review`, with a
one-sentence reason, the cost of getting the bytes back, and, opt-in, the
owning tool's own dry-run number. Staleness reads git activity as well as
source edits and says "unverifiable" rather than guessing.

**An agent can close the loop.** The MCP server grew from eight tools to
sixteen with typed output schemas and annotations. `plan_reclaim` turns a
scan into a dry-run plan and a shell script that only moves to the Trash
when *you* run it with `PHANTOM_APPLY=1`; `verify_reclaim` rescans and
reports what actually came back. The same loop is `phantom plan`, `phantom
verify`, and "Copy as script" in the app, and a bundled skill teaches an
agent the rules: safe means go after confirmation, caution means read the
why first, review means never.

**Where is "System Data"?** `phantom volume` and `get_volume_status`
decompose the gap between used and scanned: the other volumes in the APFS
container, purgeable space (what Finder's "Available" quietly includes),
local Time Machine snapshots, other users' homes, unreadable entries, and
the remainder your scan did not see. Phantom names the `tmutil` command that
thins snapshots and never runs it.

**Is it getting worse?** Every scan was already kept; now that history is a
sparkline beside each scan, a History pane with a linear "disk full in N
days" forecast that always travels with its caveat, marks on what is new or
grew since the last scan, and a compare picker for any two scans. `phantom
growth` and `phantom diff --since 7d` do the same from a shell.

**Installing.** The DMG's app now carries its own notarization ticket (a
Sparkle update or an offline first launch no longer needs an online lookup).
`brew install --cask tedswinyar/tap/phantom` puts `phantom` and
`phantom-mcp` on your PATH with completions and a man page; this repository
is a Claude Code plugin marketplace; a Claude Desktop bundle and a Cursor
one-click are in the README. Retention is now the newest 25 scans of each
root and 100 overall, so one folder's history is never evicted by another's.

Phantom still never deletes anything.

### Added

- Reuse another project's keychain and notary key — import-signing-material --reuse-from, unlock reads the conf; the MBP already serves Banshee (phantom-cnr.9)
- Everything goes through the MBP runner — relay, runner CI, releases from release/X.Y.Z (phantom-cnr.9); v1.1.1 storage-hygiene plan (phantom-cnr)
- Bounded auto-restart of the API child — 3 restarts a minute with 1/2/4 s backoff, then the failed screen (phantom-rrw)
- Curated highlights spliced under each version in the generated changelog; the 1.1.0 highlights
- Retention is per root, then total — the newest 25 scans of each root and 100 overall, operator-overridable (phantom-9tt)
- Homebrew cask renderer, shell completions + man page in the bundle, MCPB bundle, MCP Registry server.json, Claude Code plugin at the repo root, Cursor deeplink (phantom-mkn.18)
- Delta + compare — new-since-last-scan highlighting in tree/treemap, phantom diff --since, compare any two scans (phantom-mkn.23.1)
- Growth series + linear forecast — GET /scans/series, phantom growth, MCP get_growth, History tab + sidebar sparkline (phantom-mkn.11)
- Hidden-space decomposition on GET /volume — purgeable, this volume vs container, used − scanned (phantom-mkn.12, phantom-4p3)
- Insight tools — explain_path, find_stale_projects, get_volume_status on HTTP, CLI and MCP (phantom-mkn.9)
- Reclaim plans — plan_reclaim/verify_reclaim, phantom plan/verify, Copy as script, SKILL.md (phantom-mkn.7)
- Async scans — scan_status, cancel_scan, notifications/progress, 60s wait cap (phantom-mkn.8)
- Persist running scans; cold start marks orphans interrupted — schema v6, failureReason on the wire (phantom-aoa)
- Protocol currency — negotiation, annotations, outputSchema/structuredContent, concise format, result budget (phantom-mkn.6)
- Settings window (Cmd-,) — spooky verbiage is now opt-in, default OFF (phantom-2db)
- Remove a scan from the sidebar — context menu, Delete key, confirmation (phantom-9xx)
- Classifier honesty — project table, tiers, git-aware staleness, opt-in probes (phantom-mkn.3, phantom-mkn.4, phantom-mkn.5, phantom-mkn.19, phantom-mkn.21)
- Getattrlistbulk walker with APFS clone awareness — three sizes, schema v5, OPE 1.1.0 (phantom-mkn.1, phantom-mkn.2, phantom-jsz, phantom-mkn.20)

### Fixed

- Neutral names in the volume fixture and the view test — the public recut's identifier sweep refused v1.1.0
- A CoreFoundation capacity below f_bavail is no answer — purgeableBytes is null for a headless user instead of a confident 0 (found by the first gate run on the MBP runner)
- Scrub the git hook's GIT_DIR before running suites — from a worktree it is absolute and the tests' temp-repo git commands operate on the REAL repository (phantom-cnr.9 follow-up)
- Release.yml sets PHANTOM_RELEASE_CONF at runtime — the env context is not available in a job-level env block, and GitHub refused to parse the workflow (listed by path, not by name); ignore .claude/worktrees (phantom-cnr.9)
- Refuse to verify a sibling plan instead of answering from the root delta (phantom-9wc, phantom-o2f docs)
- Fail run_scripts loudly when the test-*.sh glob matches nothing (spooky-shell-rlm backport)
- Estimate only included paths (phantom-0a7)
- Hold back paths inside a git work tree that its .gitignore does not ignore; skipped.tracked counts them, no subprocess (phantom-2lz)
- Library/Caches is planned per child — the directory itself cannot be renamed on macOS (phantom-11h)
- The reclaim script reports a failed move and continues; ends with moved/failed/skipped counts and exits 1 only if something failed (phantom-lel)
- Verify's headline is Σ per-item actuals when the plan's Trash folder sits inside the root; CLI says freed/grew, and that the space returns when the Trash is emptied (phantom-grw)
- Phantom's default ports move to the Spooky Squad range — prod 18770, dev 18780 (were 8768/8778)
- Release.sh also requires rust/Cargo.toml's workspace version to match — GET /health reports CARGO_PKG_VERSION as the contract version and it still said 1.0.0 on the 1.1 dogfood build
- The API publishes its bound URL; CLI and MCP discover it — another vendor's agent squatted on 8768 (phantom_core::discovery)
- Clamp persisted magnitudes at i64::MAX, bit-cast identifiers, PB/EB size rungs; bounded LinkCharger with a saturation flag (phantom-ap6, phantom-d1h, phantom-ars)
- Release.sh blocks when the Claude Code plugin manifests do not carry the release version (phantom-mkn.18 follow-up)
- Staple the notarization ticket to Phantom.app itself, not only the DMG (phantom-ojj.1)
- Round the growth forecast's wire floats to hundredths — a raw f64 broke three-surface byte parity (phantom-mkn.11)
- A scan root that vanishes mid-walk FAILS the scan instead of completing a garbage partial; hostile-filesystem behaviour documented (phantom-4x5)
- Sidebar sparkline sits on the subtitle line inside ViewThatFits — the name and size never truncate; consistent sign glyphs in the compare header (Phase 4 UI smoke, phantom-mkn.23)
- Exit when the supervising app dies — PHANTOM_SUPERVISOR_PID watch (phantom-85r)

### Documentation

- After the runner cuts a release, local main is behind GitHub — fetch and fast-forward before committing (Gotcha + failure-table row, from the 1.1.0 recut fix)
- Failure-table row for the VPN/DNS tell — git push mbp cannot resolve the home hostname while both runners show online
- The path-as-name tell for a workflow GitHub could not parse (phantom-cnr.9)
- Point CLAUDE.md at the Specter card that asserts this repo's state
- Tell the runner to check for builds in flight before applying — script header + SKILL.md (phantom-djf)
- Views compile-only with one data-backed exception — PhantomViewTests semantic snapshots (phantom-ozu, Ted 2026-09-09)
- V1.2 plan — Always current (schedules, growth notification, exclusions + scan-options sheet, FSEvents only if measured, release 1.2.0); ROADMAP Next updated; beads phantom-adq.*
- V1.2 options memo — always-current history, clone-aware dedupe, or table stakes + reach; recommendation A
- Features page for 1.1 — tiers and why, plan/verify loop, APFS clones, hidden space, history/forecast, sixteen MCP tools; positioning proof rows and non-goals updated
- UI-naming copy mirrors the app's plain default (treemap, Reclaimable); the ghost brand voice stays in headline/tagline; getting-started notes the spooky-names toggle (phantom-06z)
- Phase 4 handoff, G4 gate bead, two earned Gotchas (statfs is the container; conformance apostrophes) (phantom-mkn.23)
- Two running Phantom.app copies share a bundle id — AppleScript activates the wrong one (earned 2026-09-08)
- Threat model re-answered for the scanner; opt-in subprocess rules before the code (phantom-mkn.5, phantom-mkn.19)
- ADR-0006 three sizes + one filesystem, Phase 1 gotchas, HANDOFF for Phase 2 (phantom-mkn.20)
- V1.1 phase plan — Fidelity + Agent Workflow (phantom-mkn)
- Competitive landscape 2026-09 + v1.1 direction in ROADMAP (phantom-mkn)
- Sparkle update channel + flip/PVR/live-update gates, from the 1.0.0 launch (phantom-pxt, phantom-3dr)

### Changed

- Classify 1M entries / 10k roots in 0.28 s, was 1.75 s — per-project artifact assembly was O(projects × roots) (phantom-45s)

### Other

- Set baseURL to GitHub Pages URL (Ted 2026-09-16: no custom domain, Pages is sufficient)

## [1.0.0] - 2026-09-04

### Added

- Styled installer DMG — background art, 128pt icons, drag arrow (Ted)
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

- Point .mcp.json at the app-bundled MCP binary, not a debug build
- Completion guard on every script with an EXIT trap
- Center the label band under Finder's actual label position (Ted, DMG smoke round 2)
- Lift the icon-label zone — Finder paints labels black over background pictures (Ted, DMG smoke)
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

- De-template the public doc set — self-contained docs, no dangling refs (phantom-3dr final review)
- Install is the DMG; building moves to its own contributor section (Ted)
- Reframe README around the agent-first thesis (Ted)
- Denser hero screenshot — sized window, tree expanded, row selected (Ted)
- README hero screenshot; recut-public.sh owns the public-cut contract (Ted)
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

