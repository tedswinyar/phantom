// Vocabulary — the product strings, in two voices. Code and API surfaces
// use conventional names (scan, entry, treemap); VIEWS use these tokens
// for every user-facing label, so the voice is a one-file change and never
// hardcoded in view code — the same rule as colors and spacing.
//
// The spooky voice (Haunt, Crypt, Séance…) is the v0.1 ghost theme, ported
// from PhantomTheme. It is OPT-IN: `Voice.spooky` defaults to false, so a
// fresh install reads plainly (Scan, Folder, Inspector) until the user turns
// the theme on in Settings › General. Both lexicons are pinned by
// DesignKitTests so a blanked or swapped term cannot ship as a blank button.

import Foundation
import Observation

/// One complete set of user-facing product terms. Every term is non-empty.
public struct Lexicon: Sendable, Equatable {
    /// A scan of a directory tree.
    public let scan: String
    /// In-progress scan state (progress indicators, status rows).
    public let scanning: String
    /// A single filesystem entry within a scan.
    public let fileEntry: String
    /// The treemap visualization surface.
    public let treemap: String
    /// The detail/inspector pane for a selected entry.
    public let inspector: String
    /// A large file surfaced by the scan.
    public let largeFile: String
    /// A scanned root (a volume or directory a scan was run against).
    public let volume: String
    /// The treemap view's display name.
    public let treemapView: String
    /// Sidebar section header listing scanned roots.
    public let sidebarTitle: String
    /// Empty-selection placeholder for the detail pane.
    public let noSelection: String
    /// The reclaimable-space surface: hotspot groups a user can safely
    /// remove (Phantom itself never deletes).
    public let reclaimable: String
    /// The launch placeholder while the bundled API server comes up.
    public let connecting: String

    /// The ghost theme. Names are proper nouns in the UI ("New Haunt").
    public static let spooky = Lexicon(
        scan: "Haunt",
        scanning: "Haunting…",
        fileEntry: "Apparition",
        treemap: "Ectoplasm",
        inspector: "Séance",
        largeFile: "Poltergeist",
        volume: "Crypt",
        treemapView: "Specter Map",
        sidebarTitle: "Crypts",
        noSelection: "Select a crypt to begin the séance",
        reclaimable: "Restless Spirits",
        connecting: "Summoning the server…"
    )

    /// Plain names — the default. Capitalised the same way as the spooky
    /// set so call sites that lowercase() mid-sentence read correctly in
    /// both voices.
    public static let plain = Lexicon(
        scan: "Scan",
        scanning: "Scanning…",
        fileEntry: "Item",
        treemap: "Treemap",
        inspector: "Inspector",
        largeFile: "Large File",
        volume: "Folder",
        treemapView: "Treemap",
        sidebarTitle: "Scans",
        noSelection: "Select a scan to see its results",
        reclaimable: "Reclaimable",
        connecting: "Starting the server…"
    )

    /// Every term, for invariant tests (non-empty, distinct across voices).
    public var allTerms: [String] {
        [
            scan, scanning, fileEntry, treemap, inspector, largeFile, volume,
            treemapView, sidebarTitle, noSelection, reclaimable, connecting,
        ]
    }
}

/// The user's voice preference, persisted in UserDefaults. @Observable so
/// every view that read a `Vocabulary` term during its body re-renders when
/// the toggle flips — no relaunch, no manual invalidation.
@MainActor
@Observable
public final class Voice {
    public static let shared = Voice()

    /// The UserDefaults key. Absent == false == plain voice.
    public static let defaultsKey = "spookyVerbiage"

    @ObservationIgnored private let defaults: UserDefaults

    /// True renders the ghost theme; false (the default) the plain names.
    public var spooky: Bool {
        didSet { defaults.set(spooky, forKey: Self.defaultsKey) }
    }

    public var lexicon: Lexicon { spooky ? .spooky : .plain }

    /// `defaults` is injectable so tests use a throwaway suite instead of
    /// the runner's real preferences.
    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.spooky = defaults.bool(forKey: Self.defaultsKey)
    }
}

/// The token surface views read. Each term forwards to the active voice;
/// `voice` is replaceable so tests can point it at a throwaway `Voice`.
@MainActor
public enum Vocabulary {
    public static let appName = "Phantom"

    public static var voice: Voice = .shared

    public static var scan: String { voice.lexicon.scan }
    public static var scanning: String { voice.lexicon.scanning }
    public static var fileEntry: String { voice.lexicon.fileEntry }
    public static var treemap: String { voice.lexicon.treemap }
    public static var inspector: String { voice.lexicon.inspector }
    public static var largeFile: String { voice.lexicon.largeFile }
    public static var volume: String { voice.lexicon.volume }
    public static var treemapView: String { voice.lexicon.treemapView }
    public static var sidebarTitle: String { voice.lexicon.sidebarTitle }
    public static var noSelection: String { voice.lexicon.noSelection }
    public static var reclaimable: String { voice.lexicon.reclaimable }
    public static var connecting: String { voice.lexicon.connecting }
}
