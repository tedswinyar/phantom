# Build & release pipeline

How `scripts/build-dmg.sh` produces a signed, notarized `.dmg` — with
particular attention to notarization credential management, which is the
part that bites.

## Entry points

```bash
./scripts/build-app.sh                # .app bundle only (ad-hoc unless configured)
./scripts/build-dmg.sh                # full: build → sign → DMG → notarize → staple
./scripts/build-dmg.sh dmg            # DMG + notarize only; reuses last .app
SKIP_NOTARIZE=1 ./scripts/build-dmg.sh   # signed-but-unnotarized DMG
./scripts/release.sh <version>        # alignment gates → verify → DMG → notes → tag
./scripts/publish.sh                  # deploy website + artifacts (stub until configured)
```

## Identity lives in release.conf, nowhere else

`scripts/release.conf` (gitignored; `release.conf.example` documents it)
carries `SIGNING_IDENTITY`, `NOTARIZE_PROFILE`, and deploy targets. Without
it, everything still works: ad-hoc signing, notarization skipped, DMG usable
locally. This is deliberate — a fresh clone must produce a runnable build
with zero Apple credentials.

## Notarization credentials — why this is fragile

`xcrun notarytool` stores credentials in the **data-protection keychain**,
not the legacy login keychain. Consequences (inherited knowledge, paid for
repeatedly upstream):

1. `security find-generic-password` cannot see the item. The canonical
   "is it there?" check is
   `xcrun notarytool history --keychain-profile phantom-notarize`.
2. The profile can become inaccessible to CLI tools after wake-from-sleep, a
   Touch ID timeout in a non-interactive shell, or iCloud keychain events —
   failing with `No Keychain password item found for profile: …`.

What the script does about it:

- **Pre-flight, fail fast**: the history check runs BEFORE the multi-minute
  build.
- **Self-healing**: if the pre-flight fails and `APPLE_ID`,
  `APPLE_TEAM_ID`, `APPLE_APP_SPECIFIC_PASSWORD` are exported, the profile
  is re-provisioned via `store-credentials --no-validate` and the build
  proceeds.
- **Precise failure messaging**: mid-build failures are classified —
  credential-disappeared (re-run; pre-flight heals), service rejection
  (inspect `notarytool log <id>`), transient (retry `build-dmg.sh dmg`).
  A signed-but-unnotarized DMG is always left on disk so a later retry
  can staple without rebuilding.

## One-time setup on a new machine

```bash
xcrun notarytool store-credentials phantom-notarize \
    --apple-id <apple-id> --team-id <team-id>
xcrun notarytool history --keychain-profile phantom-notarize   # verify
cp scripts/release.conf.example scripts/release.conf              # fill in
```

## Common failure modes

| Symptom | Likely cause | Recovery |
|---|---|---|
| `No Keychain password item found` at submit | data-protection keychain locked | run any `notarytool` command interactively, re-run build; or export the self-heal env vars |
| `status: Invalid` from notary | bundle violates hardened-runtime rules | `xcrun notarytool log <id> --keychain-profile phantom-notarize` |
| DMG works locally, warns on other Macs | not notarized (SKIP_NOTARIZE or no profile) | check `xcrun stapler validate dist/…dmg`; re-run `./scripts/build-dmg.sh dmg` with credentials |
| Release blocked: versions diverge | Version.swift ≠ OPE VERSION ≠ release arg | bump them deliberately; see `VERSIONING.md` |
| Push blocked by OPE gate during release prep | contract files changed without VERSION bump | bump `open-prompt-edition/VERSION` in the same push |

## Versioning

`swift/Sources/Phantom/Version.swift` is the single source of truth;
`build-app.sh` reads it into Info.plist; `release.sh` refuses to tag unless
it and `open-prompt-edition/VERSION` match the requested release. Build
number comes from `BUILD_NUMBER` env (default 1) — set it in CI-like
wrappers if sequential build numbers matter to you.
