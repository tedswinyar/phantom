// ScansModel growth behaviour (v1.1 Phase 4): the sidebar sparkline is
// computed from the scan list in hand; the History pane's series comes
// through the one boundary with the selected scan's root and the chosen
// groupBy; a 404 is "no history yet", not an error.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelGrowthTests: XCTestCase {

    private func scan(_ root: String, day: Int, size: UInt64, status: ScanStatus = .complete) -> Scan {
        let started = Date(timeIntervalSince1970: TimeInterval(1_800_000_000 + day * 86_400))
        return Scan(
            id: UUID(), rootPath: root, status: status, startedAt: started,
            finishedAt: status == .running ? nil : started, totalDiskSize: size,
            totalLogicalSize: size, fileCount: 1, dirCount: 1, errorCount: 0,
            unreadablePaths: [], progress: nil, totalPrivateSize: size, totalSharedSize: 0,
            failureReason: nil
        )
    }

    func testSparklineValuesAreCompleteScansOfTheRootOldestFirst() {
        let scans = [
            scan("/Users/a", day: 3, size: 300),
            scan("/Users/b", day: 2, size: 999),          // other root
            scan("/Users/a", day: 1, size: 100),
            scan("/Users/a", day: 2, size: 200, status: .cancelled), // not complete
            scan("/Users/a/", day: 4, size: 400),         // trailing slash is cosmetic
            scan("/Users/a", day: 5, size: 500, status: .running),
        ]
        XCTAssertEqual(ScansModel.sparklineValues(root: "/Users/a", in: scans), [100, 300, 400])
        XCTAssertEqual(ScansModel.sparklineValues(root: "/Users/a/", in: scans), [100, 300, 400])
        XCTAssertEqual(ScansModel.sparklineValues(root: "/Users/b", in: scans), [], "one point is not a trend")
        XCTAssertEqual(ScansModel.sparklineValues(root: "/nope", in: scans), [])
    }

    func testSelectingACompleteScanLoadsGrowthForItsRootAndGroupBy() async throws {
        let mock = MockAPIClient()
        let target = scan("/Users/a", day: 1, size: 100)
        mock.scans = [target]
        mock.growthToReturn = try Wire.decoder().decode(Growth.self, from: try WireFormatTests.fixture("growth.json"))
        let model = ScansModel(client: mock, connectionState: .connecting)
        await model.refresh()
        await model.select(scanID: target.id)
        XCTAssertEqual(model.growth?.points.count, 3)
        XCTAssertFalse(model.growthUnavailable)
        let req = try XCTUnwrap(mock.growthRequests.last)
        XCTAssertEqual(req.root, "/Users/a")
        XCTAssertEqual(req.groupBy, "total", "the default breakdown")

        // Changing the breakdown reloads with the wire spelling.
        model.growthGroupBy = .topLevelDir
        await model.loadGrowth()
        XCTAssertEqual(mock.growthRequests.last?.groupBy, "topLevelDir")

        // Re-selecting resets the history before reloading.
        await model.select(scanID: nil)
        XCTAssertNil(model.growth)
    }

    func testNoCompletedScanIsUnavailableNotAnError() async {
        let mock = MockAPIClient()
        let target = scan("/Users/a", day: 1, size: 100, status: .cancelled)
        mock.scans = [target]
        mock.growthToReturn = nil // the API's 404
        let model = ScansModel(client: mock, connectionState: .connecting)
        await model.refresh()
        await model.select(scanID: target.id)
        XCTAssertNil(model.growth)
        XCTAssertTrue(model.growthUnavailable)
        XCTAssertNil(model.lastError, "a 404 here is an expected state")
    }
}
