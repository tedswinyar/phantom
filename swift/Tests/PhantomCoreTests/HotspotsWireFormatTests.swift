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
        XCTAssertEqual(summary.groups.count, 3)
        XCTAssertFalse(summary.isEmpty)
        // The estimate is the server's number; clients never recompute it —
        // note it is NOT the sum of group diskSizes (cloud is excluded and
        // dedup is global).
        XCTAssertEqual(summary.reclaimEstimate, 22_548_578_304)
        XCTAssertEqual(summary.reviewDiskSize, 0)
        XCTAssertEqual(summary.cloudDataloadedLogicalSize, 154_140_672)
        XCTAssertEqual(summary.cloudDataloadedDiskSize, 147_456)
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

    func testHardlinkGapSurvivesDecode() throws {
        // The "17 GB listed, 5 GB freed" seam: the dot-cache group's listed
        // size EXCEEDS its deduped size — a swapped mapping flips this.
        let group = try XCTUnwrap(
            try decodeFixture().groups.first { $0.ruleId == "dot-cache" })
        XCTAssertEqual(group.diskSize, 5_368_709_120)
        XCTAssertEqual(group.listedDiskSize, 18_253_611_008)
        XCTAssertGreaterThan(group.listedDiskSize, group.diskSize)
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
            "cloudDataloadedLogicalSize", "cloudDataloadedDiskSize",
        ], "HotspotsSummary key set drifted")
        let groups = try XCTUnwrap(obj["groups"] as? [[String: Any]])
        let first = try XCTUnwrap(groups.first)
        XCTAssertEqual(Set(first.keys), [
            "ruleId", "label", "category", "hint", "command", "diskSize",
            "listedDiskSize", "logicalSize", "fileCount", "topPaths",
        ], "HotspotGroup key set drifted")
    }
}
