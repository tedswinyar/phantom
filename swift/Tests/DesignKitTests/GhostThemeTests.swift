// Ghost-theme token tests: the file-type color roles and the extension→role
// mapping, plus the product vocabulary. Views tint by ROLE, so these pin the
// role identities and the mapping — a re-theme edits Tokens.swift and these
// tests together, deliberately.

import XCTest
import SwiftUI
@testable import DesignKit

final class GhostThemeTests: XCTestCase {

    // The legend palette invariants (folders-tree-spec.md "Legend +
    // colors"): exactly nine slots, every hue pairwise distinct and
    // distinct from grey "other" — slot ASSIGNMENT is per-scan and lives in
    // ScansModel (tested there); these pin the fixed hue table.
    func testLegendHasExactlyNineDistinctSlots() {
        XCTAssertEqual(Palette.legend.count, 9)
        for (i, a) in Palette.legend.enumerated() {
            for (j, b) in Palette.legend.enumerated() where i < j {
                XCTAssertNotEqual(a, b, "legend slots \(i) and \(j) collapsed")
            }
            XCTAssertNotEqual(a, Palette.legendOther, "slot \(i) collides with other-grey")
        }
    }

    func testLegendColorFallsBackToOtherGrey() {
        XCTAssertEqual(Palette.legendColor(slot: nil), Palette.legendOther)
        XCTAssertEqual(Palette.legendColor(slot: 9), Palette.legendOther)
        XCTAssertEqual(Palette.legendColor(slot: -1), Palette.legendOther)
        XCTAssertEqual(Palette.legendColor(slot: 0), Palette.legend[0])
        XCTAssertEqual(Palette.legendColor(slot: 8), Palette.legend[8])
    }

    // Reclaimability categories map to semantic roles: safely-reclaimable
    // reads as success, review-only warns without suggesting, cloud
    // placeholders are informational, and unknown categories stay quiet
    // (a new server-side category must degrade, not shout).
    func testCategoryColorsMapToSemanticRoles() {
        for safe in ["staleProjectArtifact", "regenerableArtifact", "cache", "toolManagedCache"] {
            XCTAssertEqual(Palette.categoryColor(for: safe), Palette.success, safe)
        }
        for review in ["reviewFirst", "wontRegenerate"] {
            XCTAssertEqual(Palette.categoryColor(for: review), Palette.warning, review)
        }
        XCTAssertEqual(Palette.categoryColor(for: "cloudDataloaded"), Palette.info)
        XCTAssertEqual(Palette.categoryColor(for: "somethingNew"), Palette.textSecondary)
        XCTAssertEqual(Palette.categoryColor(for: nil), Palette.textSecondary)
    }

    func testWarningRoleIsDistinctFromSuccessAndError() {
        XCTAssertNotEqual(Palette.warning, Palette.success)
        XCTAssertNotEqual(Palette.warning, Palette.error)
    }

    // MARK: Vocabulary — two voices, plain by default

    /// A throwaway UserDefaults suite so these tests never read or write
    /// the runner's real preferences. Removed again on teardown.
    private func freshDefaults(_ name: String = #function) -> UserDefaults {
        let suite = "GhostThemeTests.\(name)"
        UserDefaults.standard.removePersistentDomain(forName: suite)
        let d = UserDefaults(suiteName: suite)!
        addTeardownBlock { UserDefaults.standard.removePersistentDomain(forName: suite) }
        return d
    }

    @MainActor
    func testVoiceDefaultsToPlainOnAFreshInstall() {
        let voice = Voice(defaults: freshDefaults())
        XCTAssertFalse(voice.spooky, "spooky verbiage is opt-in (Ted, 2026-09-08)")
        XCTAssertEqual(voice.lexicon, .plain)
    }

    @MainActor
    func testVoicePersistsTheToggleAndReadsItBack() {
        let defaults = freshDefaults()
        let voice = Voice(defaults: defaults)
        voice.spooky = true
        XCTAssertTrue(defaults.bool(forKey: Voice.defaultsKey))
        // A second Voice over the same store (a relaunch) sees the choice.
        XCTAssertTrue(Voice(defaults: defaults).spooky)
        XCTAssertEqual(Voice(defaults: defaults).lexicon, .spooky)
        voice.spooky = false
        XCTAssertFalse(Voice(defaults: defaults).spooky)
    }

    @MainActor
    func testVocabularyFollowsTheActiveVoice() {
        let previous = Vocabulary.voice
        defer { Vocabulary.voice = previous }
        let voice = Voice(defaults: freshDefaults())
        Vocabulary.voice = voice

        XCTAssertEqual(Vocabulary.scan, "Scan")
        XCTAssertEqual(Vocabulary.sidebarTitle, "Scans")
        XCTAssertEqual(Vocabulary.reclaimable, "Reclaimable")
        XCTAssertEqual(Vocabulary.connecting, "Starting the server…")

        voice.spooky = true
        XCTAssertEqual(Vocabulary.scan, "Haunt")
        XCTAssertEqual(Vocabulary.largeFile, "Poltergeist")
        XCTAssertEqual(Vocabulary.treemapView, "Specter Map")
        XCTAssertEqual(Vocabulary.reclaimable, "Restless Spirits")
        XCTAssertEqual(Vocabulary.connecting, "Summoning the server…")
        // The app name is the one term that never changes voice.
        XCTAssertEqual(Vocabulary.appName, "Phantom")
    }

    func testBothLexiconsAreCompleteAndActuallyDiffer() {
        // Every term is non-empty — a blanked label ships as a blank button.
        for term in Lexicon.spooky.allTerms + Lexicon.plain.allTerms {
            XCTAssertFalse(term.isEmpty)
        }
        XCTAssertEqual(Lexicon.spooky.allTerms.count, Lexicon.plain.allTerms.count)
        // Every slot differs between the voices, so a copy-paste that left a
        // spooky word in the plain set (or vice versa) is caught here.
        for (spooky, plain) in zip(Lexicon.spooky.allTerms, Lexicon.plain.allTerms) {
            XCTAssertNotEqual(spooky, plain, "a term is identical in both voices")
        }
        // Plain terms carry none of the ghost words — the whole point of
        // the default is that a new user never meets them uninvited.
        let ghostWords = ["haunt", "crypt", "séance", "spirit", "poltergeist", "apparition", "ectoplasm", "specter", "summon"]
        for term in Lexicon.plain.allTerms {
            for word in ghostWords {
                XCTAssertFalse(term.lowercased().contains(word), "plain term \"\(term)\" contains \"\(word)\"")
            }
        }
    }
}
