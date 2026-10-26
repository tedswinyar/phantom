// Reclaimable-space wire-format tests: decode the SHARED hotspots fixture
// (tests/fixtures/hotspots-summary.json) FROM RAW BYTES — the same bytes the
// Rust classify tests round-trip. If the two sides disagree, this fails first.

import XCTest
@testable import PhantomCore

final class HotspotsWireFormatTests: XCTestCase {

    private func decodeFixture() throws -> HotspotsSummary {
        try Wire.decoder().decode(
            HotspotsSummary.self,
            from: try WireFormatTests.fixture("hotspots-summary.json")
        )
    }

    func testDecodesSummaryFixtureFromRawBytes() throws {
        let summary = try decodeFixture()
        XCTAssertEqual(summary.groups.count, 4)
        XCTAssertFalse(summary.isEmpty)
        // The estimate is the server's number; clients never recompute it —
        // note it is NOT the sum of group diskSizes (cloud is excluded, dedup
        // is global, and since v1.1 it is Σ privateSize: 16 + 1 + 2 GiB).
        XCTAssertEqual(summary.reclaimEstimate, 20_401_094_656)
        XCTAssertEqual(summary.reviewDiskSize, 0)
        XCTAssertEqual(summary.cloudDataloadedLogicalSize, 154_140_672)
        XCTAssertEqual(summary.cloudDataloadedDiskSize, 147_456)
    }

    // v1.1 Phase 3: the reclaim plan, from the shared fixture's raw bytes.
    func testDecodesReclaimPlanFixtureAndEncodeCoversEveryField() throws {
        let plan = try Wire.decoder().decode(
            ReclaimPlan.self, from: try WireFormatTests.fixture("reclaim-plan.json"))
        XCTAssertEqual(plan.itemCount, 1)
        XCTAssertEqual(plan.maxTier, "safe")
        XCTAssertEqual(plan.expectedFreedBytes, 17_179_869_184)
        XCTAssertEqual(plan.skipped, PlanSkipped(review: 1, aboveTier: 2, belowMinBytes: 0, tracked: 0))
        let item = try XCTUnwrap(plan.items.first)
        XCTAssertEqual(item.ruleId, "cargo-target")
        XCTAssertEqual(item.command, "cargo clean")
        XCTAssertEqual(item.paths, ["/Users/ghost/Code/dormant/target"])
        XCTAssertEqual(item.expectedFreedBytes, 17_179_869_184)

        // Encode reproduces the fixture's key sets at every depth (camelCase,
        // nullable command present).
        let data = try Wire.encoder().encode(plan)
        let obj = try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let raw = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: try WireFormatTests.fixture("reclaim-plan.json")) as? [String: Any])
        XCTAssertEqual(Set(obj.keys), Set(raw.keys))
        let items = try XCTUnwrap(obj["items"] as? [[String: Any]])
        let rawItems = try XCTUnwrap(raw["items"] as? [[String: Any]])
        XCTAssertEqual(Set(items[0].keys), Set(rawItems[0].keys))
        XCTAssertEqual(obj["planId"] as? String, "7d0f3a9c-2b41-4c8e-9f65-0a1b2c3d4e5f", "uuid lowercase out")
    }

    // Phase 3: per-project activity rides the summary; an unverifiable root
    // carries null days; a pre-Phase-3 body without the key still decodes.
    func testDecodesProjectActivityAndToleratesItsAbsence() throws {
        let summary = try decodeFixture()
        XCTAssertEqual(summary.projects.count, 3)
        let dormant = try XCTUnwrap(summary.projects.first)
        XCTAssertEqual(dormant.root, "/Users/ghost/Code/dormant")
        XCTAssertEqual(dormant.lastActivityDays, 120)
        XCTAssertTrue(dormant.dormant)
        XCTAssertEqual(dormant.artifacts.map(\.ruleId), ["cargo-target"])
        XCTAssertEqual(dormant.artifacts.first?.diskSize, 17_179_869_184)
        let undated = try XCTUnwrap(summary.projects.last)
        XCTAssertNil(undated.lastActivityDays)
        XCTAssertFalse(undated.dormant)

        var obj = try JSONSerialization.jsonObject(
            with: try WireFormatTests.fixture("hotspots-summary.json")) as! [String: Any]
        obj.removeValue(forKey: "projects")
        let old = try Wire.decoder().decode(
            HotspotsSummary.self, from: try JSONSerialization.data(withJSONObject: obj))
        XCTAssertEqual(old.projects, [], "a pre-Phase-3 summary decodes with no projects")
    }

    func testDecodesStaleProjectGroup() throws {
        let group = try XCTUnwrap(try decodeFixture().groups.first)
        XCTAssertEqual(group.ruleId, "cargo-target")
        XCTAssertEqual(group.label, "Rust target/ directories")
        XCTAssertEqual(group.category, "staleProjectArtifact")
        // diskSize (deduped) is THE number; logical differs on purpose so a
        // swapped mapping cannot pass.
        XCTAssertEqual(group.diskSize, 17_179_869_184)
        XCTAssertEqual(group.listedDiskSize, 17_179_869_184)
        XCTAssertEqual(group.logicalSize, 18_179_869_184)
        XCTAssertEqual(group.fileCount, 5120)
        XCTAssertEqual(group.topPaths, ["/Users/ghost/Code/dormant/target"])
        XCTAssertFalse(group.isCloudDataloaded)
        XCTAssertFalse(group.isReviewOnly)
    }

    // v1.1 Phase 2: the honesty fields, from the raw bytes — the tier is a
    // stated fact, the why ends the sentence, the rebuild cost has a kind.
    func testTierWhyAndRebuildCostDecodeFromRawBytes() throws {
        let summary = try decodeFixture()
        let cargo = try XCTUnwrap(summary.groups.first { $0.ruleId == "cargo-target" })
        XCTAssertEqual(cargo.riskTier, "safe")
        XCTAssertTrue(cargo.isSafe)
        XCTAssertFalse(cargo.isCaution)
        XCTAssertFalse(cargo.isReviewTier)
        XCTAssertTrue(cargo.why.hasSuffix("at least 90 days old."), cargo.why)
        XCTAssertEqual(cargo.rebuildCost, RebuildCost(kind: "compile", estimate: "re-compile ≈ 17.2 GB of build output"))
        XCTAssertNil(cargo.toolEstimate)

        let cache = try XCTUnwrap(summary.groups.first { $0.ruleId == "dot-cache" })
        XCTAssertEqual(cache.riskTier, "caution")
        XCTAssertTrue(cache.isCaution)
        XCTAssertEqual(cache.rebuildCost.kind, "download")

        let brew = try XCTUnwrap(summary.groups.first { $0.ruleId == "homebrew-cellar" })
        XCTAssertEqual(
            brew.toolEstimate,
            ToolEstimate(
                tool: "brew", command: "brew cleanup -n", reclaimableBytes: 734_003_200,
                note: "superseded formula versions and downloads brew would remove; the Cellar itself stays"))

        let cloud = try XCTUnwrap(summary.groups.first { $0.ruleId == "cloud-dataloaded" })
        XCTAssertEqual(cloud.riskTier, "review")
        XCTAssertTrue(cloud.isReviewTier)
        XCTAssertEqual(cloud.rebuildCost.kind, "none")
    }

    /// A summary persisted before v1.1 has none of the tier fields: it
    /// decodes as UNRATED — review, a legacy why, a none rebuild — never as
    /// safe. And an unknown tier string is not safe either.
    func testMissingTierFieldsDecodeAsUnratedNeverSafe() throws {
        var raw = String(decoding: try WireFormatTests.fixture("hotspots-summary.json"), as: UTF8.self)
        for key in ["riskTier", "why", "rebuildCost", "toolEstimate"] {
            XCTAssertTrue(raw.contains("\"\(key)\""), "precondition: \(key) in the fixture")
        }
        // Strip the four keys from every group with a line-based edit: each
        // key sits on its own line (rebuildCost spans four).
        var lines = raw.components(separatedBy: "\n")
        var out: [String] = []
        var skipping = 0
        for line in lines {
            if skipping > 0 { skipping -= 1; continue }
            if line.contains("\"rebuildCost\"") { skipping = 3; continue }
            if line.contains("\"toolEstimate\": {") { skipping = 5; continue }
            if line.contains("\"riskTier\"") || line.contains("\"why\"") || line.contains("\"toolEstimate\"") { continue }
            out.append(line)
        }
        lines = out
        raw = lines.joined(separator: "\n")
        for key in ["riskTier", "why", "rebuildCost", "toolEstimate"] {
            XCTAssertFalse(raw.contains("\"\(key)\""), "\(key) must be gone")
        }
        let summary = try Wire.decoder().decode(HotspotsSummary.self, from: Data(raw.utf8))
        XCTAssertEqual(summary.groups.count, 4)
        let group = try XCTUnwrap(summary.groups.first)
        XCTAssertEqual(group.riskTier, "review")
        XCTAssertTrue(group.isReviewTier)
        XCTAssertTrue(group.why.contains("classified before v1.1"), group.why)
        XCTAssertEqual(group.rebuildCost.kind, "none")
        XCTAssertNil(group.toolEstimate)

        let odd = HotspotGroup(
            ruleId: "r", label: "l", category: "cache", hint: "h", riskTier: "definitely-fine",
            diskSize: 1, listedDiskSize: 1, logicalSize: 1, fileCount: 1, topPaths: [])
        XCTAssertTrue(odd.isReviewTier, "an unknown tier string is unrated, not safe")
        XCTAssertFalse(odd.isSafe)
    }

    func testModelCacheCategoryIsRecognized() {
        let group = HotspotGroup(
            ruleId: "ollama-models", label: "l", category: "modelCache", hint: "h", riskTier: "caution",
            diskSize: 1, listedDiskSize: 1, logicalSize: 1, fileCount: 1, topPaths: [])
        XCTAssertTrue(group.isModelCache)
        XCTAssertFalse(group.isReviewOnly)
        XCTAssertFalse(group.isCloudDataloaded)
    }

    func testHardlinkGapSurvivesDecode() throws {
        // The "17 GB listed, 5 GB freed" seam: the dot-cache group's listed
        // size EXCEEDS its deduped size — a swapped mapping flips this.
        let group = try XCTUnwrap(
            try decodeFixture().groups.first { $0.ruleId == "dot-cache" })
        XCTAssertEqual(group.diskSize, 5_368_709_120)
        XCTAssertEqual(group.listedDiskSize, 18_253_611_008)
        XCTAssertGreaterThan(group.listedDiskSize, group.diskSize)
        // v1.1: du says 5 GB; deleting the store frees 1 GB (the venvs pin
        // the rest). privateSize ≤ diskSize ≤ listedDiskSize.
        XCTAssertEqual(group.privateSize, 1_073_741_824)
        XCTAssertLessThan(group.privateSize, group.diskSize)
    }

    func testCloudDataloadedGroupIsInformational() throws {
        let group = try XCTUnwrap(
            try decodeFixture().groups.first { $0.ruleId == "cloud-dataloaded" })
        XCTAssertTrue(group.isCloudDataloaded)
        // The du-lie in miniature: huge logical claim, ~zero blocks.
        XCTAssertEqual(group.logicalSize, 154_140_672)
        XCTAssertEqual(group.diskSize, 147_456)
    }

    // The command is FIRST-CLASS on the wire (freeze review R1): decoded,
    // never parsed out of the hint. The dot-cache hint still contains
    // backticked commands as typography — its command is null, and it must
    // STAY null through decode: the old regex extraction is impossible now.
    func testCommandComesFromTheWireNotTheHint() throws {
        let summary = try decodeFixture()
        let cargo = try XCTUnwrap(summary.groups.first { $0.ruleId == "cargo-target" })
        XCTAssertEqual(cargo.command, "cargo clean")
        let cache = try XCTUnwrap(summary.groups.first { $0.ruleId == "dot-cache" })
        XCTAssertTrue(cache.hint.contains("`"), "precondition: backticks in the hint")
        XCTAssertNil(cache.command,
                     "backticked hint text must NOT surface as a command — no copy affordance")
        let cloud = try XCTUnwrap(summary.groups.first { $0.ruleId == "cloud-dataloaded" })
        XCTAssertNil(cloud.command)
    }

    func testNullCommandEncodesPresentAsNull() throws {
        let summary = try decodeFixture()
        let data = try Wire.encoder().encode(summary)
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let groups = try XCTUnwrap(obj["groups"] as? [[String: Any]])
        let noCommand = try XCTUnwrap(groups.first { $0["ruleId"] as? String == "dot-cache" })
        let value = try XCTUnwrap(noCommand["command"], "command must be PRESENT as null")
        XCTAssertTrue(value is NSNull)
        // toolEstimate follows the same rule: null is PRESENT, an estimate is an object.
        let estimate = try XCTUnwrap(noCommand["toolEstimate"], "toolEstimate must be PRESENT as null")
        XCTAssertTrue(estimate is NSNull)
        let brew = try XCTUnwrap(groups.first { $0["ruleId"] as? String == "homebrew-cellar" })
        let tool = try XCTUnwrap(brew["toolEstimate"] as? [String: Any])
        XCTAssertEqual(Set(tool.keys), ["tool", "command", "reclaimableBytes", "note"], "ToolEstimate key set drifted")
        let rebuild = try XCTUnwrap(brew["rebuildCost"] as? [String: Any])
        XCTAssertEqual(Set(rebuild.keys), ["kind", "estimate"], "RebuildCost key set drifted")
    }

    func testReviewOnlyCategoriesAreFlagged() {
        for category in ["reviewFirst", "wontRegenerate"] {
            let group = HotspotGroup(
                ruleId: "r", label: "l", category: category, hint: "h",
                diskSize: 1, listedDiskSize: 1, logicalSize: 1, fileCount: 1,
                topPaths: [])
            XCTAssertTrue(group.isReviewOnly, "\(category) must be review-only")
            XCTAssertFalse(group.isCloudDataloaded)
        }
    }

    func testDecodesEmptySummaryHonestly() throws {
        // The API's terminal default when nothing was classified.
        let raw = Data("""
        {
            "groups": [],
            "reclaimEstimate": 0,
            "reviewDiskSize": 0,
            "cloudDataloadedLogicalSize": 0,
            "cloudDataloadedDiskSize": 0
        }
        """.utf8)
        let summary = try Wire.decoder().decode(HotspotsSummary.self, from: raw)
        XCTAssertTrue(summary.isEmpty)
        XCTAssertEqual(summary.reclaimEstimate, 0)
    }

    // Key-set drift pin (same shape as the other wire types'): encode the
    // decoded fixture and fail if the emitted key sets drift from the
    // contract at either depth.
    func testEncodeKeySetsMatchTheContract() throws {
        let summary = try decodeFixture()
        let data = try Wire.encoder().encode(summary)
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(Set(obj.keys), [
            "groups", "reclaimEstimate", "reviewDiskSize",
            "cloudDataloadedLogicalSize", "cloudDataloadedDiskSize", "projects",
        ], "HotspotsSummary key set drifted")
        let projects = try XCTUnwrap(obj["projects"] as? [[String: Any]])
        XCTAssertEqual(Set(try XCTUnwrap(projects.first).keys), ["root", "lastActivityDays", "dormant", "artifacts"])
        let groups = try XCTUnwrap(obj["groups"] as? [[String: Any]])
        let first = try XCTUnwrap(groups.first)
        XCTAssertEqual(Set(first.keys), [
            "ruleId", "label", "category", "hint", "command",
            "riskTier", "why", "rebuildCost", "toolEstimate", "diskSize",
            "listedDiskSize", "privateSize", "logicalSize", "fileCount", "topPaths",
        ], "HotspotGroup key set drifted")
    }
}
