// The single source of truth for the app's release version.
// scripts/build-app.sh reads it; scripts/release.sh enforces that it,
// CHANGELOG.md, and open-prompt-edition/VERSION agree before tagging.
// (VERSIONING.md documents the policy.)

enum Version {
    static let marketing = "1.0.0"
}
