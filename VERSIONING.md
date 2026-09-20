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

**App and OPE share the same version number.**

When you bump the app version, bump OPE to match. These must contain the same
version at release time:

| File | Purpose |
|------|---------|
| `swift/Sources/Phantom/Version.swift` | App runtime version (single source of truth) |
| `open-prompt-edition/VERSION` | OPE kit version |
| `scripts/build-app.sh` → `VERSION` | Build script (reads from Version.swift) |

`scripts/release.sh` enforces alignment and blocks the release if versions
diverge. `scripts/check-ope-version-bump.sh` (run by the pre-push gate)
additionally blocks pushes that change OPE contract files without bumping
`open-prompt-edition/VERSION`.

Cargo crate versions are independent of the app release and are not bumped
per release.

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
2. **Update Version.swift** — change `Version.marketing`
3. **Update open-prompt-edition/VERSION** — must match
4. **Run**: `./scripts/release.sh <version>`
5. **Script validates alignment** → builds → signs → notarizes → tags

## Rules

| Do | Don't |
|----|-------|
| Bump version in both files | Ship a DMG without bumping the version |
| Use semantic versioning | Bump OPE major without app major |
| Create annotated git tags | Skip version numbers (0.2.0 → 0.4.0) |
