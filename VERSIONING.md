# Phantom Versioning Policy

## Semantic Versioning

Phantom uses [Semantic Versioning](https://semver.org/): `MAJOR.MINOR.PATCH`

| Bump | Trigger | Example |
|------|---------|---------|
| **Major** | **Breaking** contract changes only: a field removed or retyped, an endpoint's semantics changed, a migration existing readers cannot survive | 1.0.0 → 2.0.0 |
| **Minor** | Additive changes: new features, new endpoints, new nullable fields, **forward-only additive schema migrations**, new OPE prompts/specs | 0.2.0 → 0.3.0 |
| **Patch** | Bug fixes, security fixes, documentation improvements, conformance test fixes | 0.2.0 → 0.2.1 |

The dividing line is compatibility, not mechanism: an additive nullable
column with a forward-only migration is Minor even though it migrates the
schema. If the table ever points at two rows at once, the change is Minor
unless something existing breaks. (Clarified after a stamped project hit
the ambiguity, 2026-08-19.)

## "Is this breaking?" Decision Tree

Edge cases and worked examples:

**Adding validation that tightens what's accepted**
```
Q: Does existing valid data become invalid?
   Yes → Major (breaks stored data)
   No → Minor (tightens input only, DB unaffected)
```
Example: Adding `1 <= priority <= 3` validation when the DB already enforces it → Minor

**Fixing a bug that clients may have depended on**
```
Q: Was the bug documented/intentional behavior?
   Yes → Major (documented behavior changed)
   No → Is the fix in an API response?
      Yes → Could break clients → Minor (document in changelog)
      No → Patch (internal fix)
```
Example: Fixing timestamp rounding that was never specified → Minor + changelog

**Changing HTTP status codes**
```
Q: Is the new status code more correct per HTTP semantics?
   Yes + both are 4xx or both are 5xx → Minor (clarification)
   No or crosses 4xx/5xx boundary → Major (semantics changed)
```
Example: Changing 400 → 422 for validation failures → Minor (both client errors)
Example: Changing 500 → 400 when it was actually a bad request → Minor (fix)
Example: Changing 200 → 201 for creates → Minor (more correct)

**Changing log levels or error messages**
```
Log-level changes → Patch (internal diagnostics)
Error message text (not structure) → Patch (not part of wire contract)
Error message structure (new field, different key) → Minor if additive, Major if breaking
```

**Renaming internal functions, files, or modules**
```
Q: Is it in the public API (HTTP endpoints, MCP tools, CLI flags)?
   Yes → Major (breaks clients)
   No → Patch (internal refactor)
```

**Adding a required field with a server-side default**
```
Additive from client perspective (they don't send it) → Minor
```
Example: Adding `created_at` field populated by the server → Minor

**Changing field types**
```
Wider type (i32 → i64) + wire format stays JSON number → Minor (compatible)
Narrower type (i64 → i32) → Major (could truncate)
Type category change (string → number) → Major (breaks parsing)
```

## Version Alignment

**Every source of "what version is this?" moves TOGETHER, at the START of a
release cycle, to the version being assembled.** (Policy since 2026-09-20,
phantom-cnr.6: `rust/Cargo.toml` sat at 1.0.0 for two weeks of 1.1 work, so
`phantom --version`, `phantom-mcp` and `GET /health` all reported 1.0.0 on
builds carrying every post-1.0 fix, and nothing on the push path noticed.)

| File | Read by |
|------|---------|
| `swift/Sources/Phantom/Version.swift` | the app (`CFBundleShortVersionString`, Sparkle), `build-app.sh` |
| `rust/Cargo.toml` `[workspace.package] version` | `GET /health` (the OPE calls this the contract version), `--version`, MCP `serverInfo` |
| `open-prompt-edition/VERSION` | the OPE kit and its conformance gate |
| `.claude-plugin/plugin.json` `.version` | the Claude Code plugin marketplace (users only update when it moves) |
| `.claude-plugin/marketplace.json` `.plugins[0].version` | same |
| `CHANGELOG.md` top heading | `[Unreleased]` during a cycle, or exactly the version once released (git-cliff writes it at release) |

Three checks, in order of when they fire:

1. `scripts/check-version-alignment.sh` — runs in `verify.sh`'s scripts
   suite, so on every pre-push: the five sources must agree and the CHANGELOG
   heading must be `[Unreleased]` or that version. Fails naming each source
   and its value. Tested by `scripts/tests/test-version-alignment.sh`.
2. `scripts/check-ope-version-bump.sh` — the pre-push gate blocks a push that
   changes OPE contract files without bumping `open-prompt-edition/VERSION`
   above the last `v*` tag (one release, one bump).
3. `scripts/release.sh <version>` — the last line: every source must equal the
   version being cut, and the bundled CLI must print exactly `phantom <version>`.

**Untagged builds say so.** `rust/build-support/git-describe.rs` (one build
script for `phantom`, `phantom-mcp`, `phantom-api`) embeds `git describe --tags
--dirty --always`: `--version` prints `phantom 1.1.1 (v1.1.0-14-g3896576-dirty)`
on a development build and `phantom 1.1.1` on the release, which `release.sh`
produces by exporting `PHANTOM_GIT_DESCRIBE=v<version>` for the build (the DMG
is built before the tag exists) and asserting the output. `GET /health` and MCP
`serverInfo.version` stay the plain Cargo version — a build key on the wire is
additive and waits for v1.2.

## Version Files

### Version.swift (authoritative)
```swift
enum Version {
    static let marketing = "0.1.0"
}
```

### Info.plist (at build time)
- `CFBundleShortVersionString` → marketing version (from Version.swift)
- `CFBundleVersion` → build number (`BUILD_NUMBER` env or 1)

### Git Tags
- Format: `v{MAJOR}.{MINOR}.{PATCH}` (annotated)
- Created by `scripts/release.sh`

## Release Process

1. **Decide bump** — features = minor, fixes = patch, breaking = major
2. **First commit of the cycle: move all five sources** — Version.swift,
   `rust/Cargo.toml` (then `cargo update -w` for Cargo.lock),
   `open-prompt-edition/VERSION`, both plugin manifests. verify.sh refuses the
   push otherwise.
3. **Develop**; every push re-checks alignment and the OPE bump rule
4. **Run**: `./scripts/release.sh <version>` (on the build server: push
   `release/<version>` at main's tip — `docs/build-server.md`)
5. **Script validates alignment** → verify → builds → signs → notarizes →
   asserts the bundled CLI prints `phantom <version>` → notes → tags

## Rules

| Do | Don't |
|----|-------|
| Bump version in both files | Ship a DMG without bumping the version |
| Use semantic versioning | Bump OPE major without app major |
| Create annotated git tags | Skip version numbers (0.2.0 → 0.4.0) |
