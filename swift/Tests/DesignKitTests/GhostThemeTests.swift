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

    func testVocabularyCarriesTheGhostVoice() {
        XCTAssertEqual(Vocabulary.appName, "Phantom")
        XCTAssertEqual(Vocabulary.scan, "Haunt")
        XCTAssertEqual(Vocabulary.largeFile, "Poltergeist")
        XCTAssertEqual(Vocabulary.treemapView, "Specter Map")
        XCTAssertEqual(Vocabulary.reclaimable, "Restless Spirits")
        // Every term is non-empty — a blanked label ships as a blank button.
        for term in [
            Vocabulary.appName, Vocabulary.scan, Vocabulary.scanning,
            Vocabulary.fileEntry, Vocabulary.treemap, Vocabulary.inspector,
            Vocabulary.largeFile, Vocabulary.volume, Vocabulary.treemapView,
            Vocabulary.sidebarTitle, Vocabulary.noSelection,
        ] {
            XCTAssertFalse(term.isEmpty)
        }
    }
}
