// Vocabulary — the ghost-theme product strings, ported from the v0.1 app's
// PhantomTheme. Code and API surfaces use conventional names (scan, entry,
// treemap); VIEWS use these tokens for every user-facing label, so the
// spooky voice is a one-file change and never hardcoded in view code —
// the same rule as colors and spacing.

public enum Vocabulary {
    public static let appName = "Phantom"

    /// A scan of a directory tree.
    public static let scan = "Haunt"
    /// In-progress scan state (progress indicators, status rows).
    public static let scanning = "Haunting…"
    /// A single filesystem entry within a scan.
    public static let fileEntry = "Apparition"
    /// The treemap visualization surface.
    public static let treemap = "Ectoplasm"
    /// The detail/inspector pane for a selected entry.
    public static let inspector = "Séance"
    /// A large file surfaced by the scan.
    public static let largeFile = "Poltergeist"
    /// A scanned root (a volume or directory a scan was run against).
    public static let volume = "Crypt"
    /// The treemap view's display name.
    public static let treemapView = "Specter Map"
    /// Sidebar section header listing scanned roots.
    public static let sidebarTitle = "Crypts"
    /// Empty-selection placeholder for the detail pane.
    public static let noSelection = "Select a crypt to begin the séance"
    /// The reclaimable-space surface: hotspot groups a user can safely put
    /// to rest (the hint names the rite; Phantom itself never deletes).
    public static let reclaimable = "Restless Spirits"
}
