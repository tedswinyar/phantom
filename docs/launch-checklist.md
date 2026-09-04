# Launch checklist

Ordered. A step you skip is a step you re-do publicly.

## Before anything is public

- [ ] `docs/positioning.md` filled in (audience, hook, proof)
- [ ] `gitleaks detect --config .gitleaks.toml` clean over full history —
      **history, not just HEAD**; a secret in an old commit ships with the repo
- [ ] LICENSE year/name correct; `THIRD-PARTY-NOTICES.html` regenerated
- [ ] SECURITY.md contact reachable; threat model current
- [ ] README rewritten for the audience (the stamped default describes the
      template slice, which is not your product)

## Release artifact

- [ ] `./scripts/release.sh <version>` green (alignment + verify + DMG + tag)
- [ ] DMG **notarized** (not SKIP_NOTARIZE) and `stapler validate` passes
- [ ] Fresh-Mac test — **a HARD GATE, on a second physical Mac** (safety
      review 2026-09-01: the first-run path has only ever executed on the
      dev machine; a fresh user account misses machine-level state). The
      full protocol, in order:
      1. Download the DMG in Safari (quarantine flag matters — do not scp).
      2. Open, drag to /Applications, eject, launch from /Applications.
         Expect: Gatekeeper shows the notarized-app dialog at most; NO
         "unidentified developer" block; app window appears within ~5s.
      3. First-run internals: sidebar shows the empty state; no keyfile or
         data dir existed before (verify: `ls "~/Library/Application
         Support/phantom/"` created fresh, api_key mode 0600).
      4. Scan a plain folder (~/Code or ~/Downloads) — completes, treemap
         + tree render, no FDA prompts expected.
      5. TCC attribution check (the assumption FullDiskAccess.swift makes):
         scan ~/Library WITHOUT granting FDA — expect errorCount > 0 and
         the callout with the System Settings button; the deep link lands
         on Privacy & Security → Full Disk Access; grant, relaunch, rescan
         — expect errorCount 0 (or dramatically lower). If errors do NOT
         drop, the child-process TCC inheritance assumption is WRONG —
         stop the release and report.
      6. Quit, relaunch: previous scan appears (persistence), no orphaned
         phantom-api process left behind after quit (`pgrep phantom-api`).
- [ ] Backup restore drill (`docs/data-safety.md`):
      `cargo test -p phantom-core restore_drill -- --ignored --nocapture`
      prints `PASS`; optionally point `PHANTOM_DRILL_DB` at a STOPPED copy of
      a real database. (Note: `backup_verified` has no operational surface —
      no CLI/API path invokes it; per the v1.0 posture scan data is
      regenerable, so the drill is the whole story until backups grow one.)

## Website

- [ ] `./scripts/capture-screenshots.sh` output current (not the Note slice)
- [ ] Download page points at the real DMG; changelog regenerated
- [ ] `./scripts/publish.sh` configured and run; site loads from the real domain
- [ ] Every website claim traceable to the positioning proof table
- [ ] Footer `footer_note` and `footer_copy` replaced with real copy (not template placeholder)
- [ ] No `TODO-stamp` markers survive in built site: `hugo build && grep -ri TODO-stamp public/`

## Announce prep

- [ ] Demo asset captured: GIF or video showing the hook (e.g., agent driving the app)
- [ ] Answers prepared for obvious objections (Why not X? What about Y?)
- [ ] Distribution channel chosen (one first):
  * Show HN (if dev tool with interesting tech)
  * Relevant subreddits (r/programming, r/MacOS, etc.)
  * Product Hunt (if consumer-angle exists)
  * Community Slack/Discord where audience hangs out
  * Direct outreach to 5-10 people who match the audience profile

## Announce

- [ ] One channel first, watch for breakage, then the rest
- [ ] The announcement demos the HOOK (with the demo asset), not the feature list
- [ ] Feedback path stated (issues? email?) — silence reads as abandonment

## After

- [ ] File beads for everything that came up during launch
- [ ] Note what worked/flopped while it is fresh (for the next release)
