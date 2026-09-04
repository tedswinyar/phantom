// ScansModel tests — the scan lifecycle (start → poll → terminal), cancel,
// types (including the 409-while-running state), and treemap drill-down,
// driven through MockAPIClient (the ONE approved mock) at the network
// boundary. Polling is deterministic: pollInterval is zero and the mock's
// scripted update queue THROWS when exhausted, so a poll loop that misses
// its stop condition fails loudly instead of hanging the suite.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelTests: XCTestCase {

    private func makeScan(
        id: UUID = UUID(),
        rootPath: String = "/tmp/haunt",
        status: ScanStatus = .running,
        progress: Scan.Progress? = Scan.Progress(filesSeen: 0, bytesSeen: 0, currentPath: "/tmp/haunt")
    ) -> Scan {
        Scan(
            id: id, rootPath: rootPath, status: status, startedAt: Date(),
            finishedAt: status == .running ? nil : Date(),
            totalDiskSize: 0, totalLogicalSize: 0,
            fileCount: 0, dirCount: 0, errorCount: 0, unreadablePaths: [],
            progress: status == .running ? progress : nil
        )
    }

    // MARK: refresh

    func testRefreshLoadsScansAndConnects() async {
        let mock = MockAPIClient()
        mock.scans = [makeScan(status: .complete)]
        let model = ScansModel(client: mock, connectionState: .connecting)
        await model.refresh()
        XCTAssertEqual(model.connectionState, .connected)
        XCTAssertEqual(model.scans.count, 1)
        XCTAssertNil(model.lastError)
    }

    func testRefreshFailureSetsFailedState() async {
        let mock = MockAPIClient()
        mock.failWith = .serverUnreachable("down")
        let model = ScansModel(client: mock, connectionState: .connecting)
        await model.refresh()
        guard case .failed = model.connectionState else {
            return XCTFail("expected .failed, got \(model.connectionState)")
        }
    }

    // MARK: start + poll

    func testStartScanInsertsRunningScanAndPollsToTerminal() async throws {
        let mock = MockAPIClient()
        let model = ScansModel(client: mock)
        await model.startScan(rootPath: "/tmp/haunt")

        let started = try XCTUnwrap(model.scans.first)
        XCTAssertEqual(started.status, .running)
        XCTAssertNotNil(started.progress, "202 view carries live progress")

        // Script the poll timeline BEFORE the poll task gets the main actor:
        // one live snapshot, then the terminal row. (This runs synchronously
        // after startScan returns, so the poller cannot have polled yet.)
        var mid = started
        mid.progress = Scan.Progress(filesSeen: 500, bytesSeen: 1024, currentPath: "/tmp/haunt/x")
        var done = started
        done.status = .complete
        done.finishedAt = Date()
        done.progress = nil
        done.totalDiskSize = 4096
        mock.scanUpdates[started.id] = [mid, done]

        await model.awaitPolling(id: started.id)

        let final = try XCTUnwrap(model.scans.first)
        XCTAssertEqual(final.status, .complete)
        XCTAssertNil(final.progress, "terminal scan has null progress")
        XCTAssertEqual(final.totalDiskSize, 4096)
        XCTAssertNil(model.lastError)
        // Exactly one poll per scripted snapshot: stops AT the terminal one,
        // neither early (status would still be running) nor late (the
        // exhausted queue would throw and set lastError).
        XCTAssertEqual(mock.getScanCallCount, 2)
    }

    func testPollUpdatesLiveProgressCounters() async throws {
        // refresh (not startScan) so no background poll task competes with
        // the direct pollUntilTerminal call for the scripted queue.
        let mock = MockAPIClient()
        let running = makeScan()
        mock.scans = [running]
        let model = ScansModel(client: mock)
        await model.refresh()

        var mid = running
        mid.progress = Scan.Progress(filesSeen: 1337, bytesSeen: 987, currentPath: "/tmp/haunt/deep")
        mock.scanUpdates[running.id] = [mid]

        // Drive the poll loop directly: it reads the snapshot, upserts it,
        // then errors on the exhausted queue and stops.
        await model.pollUntilTerminal(id: running.id)
        // The live counters from the mid-flight snapshot reached the row.
        XCTAssertEqual(model.scans.first?.progress?.filesSeen, 1337)
        XCTAssertNotNil(model.lastError, "exhausted queue surfaces as an error")
    }

    func testPollErrorSurfacesAndStops() async throws {
        let mock = MockAPIClient()
        let model = ScansModel(client: mock)
        await model.startScan(rootPath: "/tmp/haunt")
        let started = try XCTUnwrap(model.scans.first)
        mock.scanUpdates[started.id] = [] // immediately exhausted → throws

        await model.awaitPolling(id: started.id)
        XCTAssertNotNil(model.lastError)
        XCTAssertEqual(model.scans.first?.status, .running, "row untouched on poll failure")
    }

    func testStartScanFailureSurfacesError() async {
        let mock = MockAPIClient()
        mock.failWith = .httpError(status: 400, message: "not a directory: /nope")
        let model = ScansModel(client: mock)
        await model.startScan(rootPath: "/nope")
        XCTAssertTrue(model.scans.isEmpty)
        XCTAssertEqual(model.lastError, "not a directory: /nope (400)")
    }

    // MARK: cancel

    func testCancelRunningScanReconcilesRow() async throws {
        let mock = MockAPIClient()
        let running = makeScan()
        mock.scans = [running]
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.cancel(id: running.id)
        let row = try XCTUnwrap(model.scans.first)
        XCTAssertEqual(row.status, .cancelled)
        XCTAssertNil(row.progress)
        XCTAssertNil(model.lastError)
    }

    func testCancelTerminalScanSurfaces409WithoutTouchingRow() async throws {
        let mock = MockAPIClient()
        let complete = makeScan(status: .complete)
        mock.scans = [complete]
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.cancel(id: complete.id)
        let message = try XCTUnwrap(model.lastError)
        XCTAssertTrue(message.contains("already complete"), "got: \(message)")
        XCTAssertEqual(model.scans.first?.status, .complete, "409 must not mutate the row")
    }

    // MARK: types (409 while running is a state, not an error)

    func testLoadTypesLoadsTotals() async {
        let mock = MockAPIClient()
        let totals = [
            FileTypeTotal(fileType: "rs", diskSize: 1_048_576, fileCount: 42),
            FileTypeTotal(fileType: nil, diskSize: 4096, fileCount: 3),
        ]
        mock.typeTotals = totals
        let model = ScansModel(client: mock)
        await model.loadTypes(scanID: UUID())
        XCTAssertEqual(model.typeTotals, totals)
        XCTAssertFalse(model.typesStillRunning)
        XCTAssertNil(model.lastError)
    }

    func testLoadTypesWhileRunningSetsPendingStateNotError() async {
        let mock = MockAPIClient()
        mock.typesConflict = true
        let model = ScansModel(client: mock)
        await model.loadTypes(scanID: UUID())
        XCTAssertTrue(model.typesStillRunning)
        XCTAssertTrue(model.typeTotals.isEmpty)
        XCTAssertNil(model.lastError, "409-while-running is an expected state, not an error")
    }

    func testLoadTypesDecodeFailureSurfacesError() async {
        let mock = MockAPIClient()
        mock.failWith = .decodingFailed("garbage bytes")
        let model = ScansModel(client: mock)
        await model.loadTypes(scanID: UUID())
        XCTAssertNotNil(model.lastError)
        XCTAssertFalse(model.typesStillRunning)
    }

    // MARK: treemap drill-down

    private func makeLayout(root: String, size: UInt64) -> TreemapLayout {
        TreemapLayout(rootPath: root, totalSize: size, rects: [
            TreemapRect(
                path: root, name: String(root.split(separator: "/").last ?? "root"),
                size: size, x: 0, y: 0, width: 800, height: 600, depth: 0,
                isDir: true, fileType: nil),
        ])
    }

    func testTreemapDrillInAndOut() async {
        let mock = MockAPIClient()
        let rootLayout = makeLayout(root: "/Users/ghost/Code", size: 3_145_728)
        let subLayout = makeLayout(root: "/Users/ghost/Code/sub", size: 2_097_152)
        mock.treemapsByRoot = [nil: rootLayout, "/Users/ghost/Code/sub": subLayout]
        let model = ScansModel(client: mock)
        let scanID = UUID()

        await model.loadTreemap(scanID: scanID, width: 800, height: 600)
        XCTAssertEqual(model.treemap, rootLayout)
        XCTAssertNil(model.treemapRoot, "initial load is rooted at the scan root")
        XCTAssertEqual(mock.treemapRootsRequested, [nil], "scan-root load sends no root=")
        XCTAssertEqual(mock.lastTreemapWidth, 800, "the app's view size reaches the wire")
        XCTAssertEqual(mock.lastTreemapHeight, 600)

        await model.drillIn(to: "/Users/ghost/Code/sub")
        XCTAssertEqual(model.treemap, subLayout)
        XCTAssertEqual(model.treemapRoot, "/Users/ghost/Code/sub")
        XCTAssertEqual(mock.treemapRootsRequested, [nil, "/Users/ghost/Code/sub"])

        await model.drillOut()
        XCTAssertEqual(model.treemap, rootLayout)
        XCTAssertNil(model.treemapRoot)
        XCTAssertEqual(mock.treemapRootsRequested, [nil, "/Users/ghost/Code/sub", nil])
        XCTAssertNil(model.lastError)
    }

    func testDrillInFailureKeepsCurrentRootAndLayout() async {
        let mock = MockAPIClient()
        let rootLayout = makeLayout(root: "/Users/ghost/Code", size: 3_145_728)
        mock.treemapsByRoot = [nil: rootLayout] // the sub path is NOT served → 404
        let model = ScansModel(client: mock)
        await model.loadTreemap(scanID: UUID())

        await model.drillIn(to: "/Users/ghost/Code/missing")
        XCTAssertNotNil(model.lastError)
        XCTAssertNil(model.treemapRoot, "failed drill must not move the drill stack")
        XCTAssertEqual(model.treemap, rootLayout, "failed drill must not clear the layout")
    }

    func testDrillOutAtScanRootIsANoOp() async {
        let mock = MockAPIClient()
        mock.treemapsByRoot = [nil: makeLayout(root: "/x", size: 1)]
        let model = ScansModel(client: mock)
        await model.loadTreemap(scanID: UUID())

        await model.drillOut()
        XCTAssertEqual(mock.treemapRootsRequested, [nil], "no refetch when already at the root")
        XCTAssertNil(model.lastError)
    }
}
