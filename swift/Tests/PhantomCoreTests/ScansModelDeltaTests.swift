// Delta and compare (v1.1 Phase 4, phantom-mkn.23.1): the previous-scan
// baseline is chosen from the scan list in hand; the mark for a path comes
// from the server's grown list and never touches the root; comparing any
// two scans always asks older → newer.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelDeltaTests: XCTestCase {

    private func scan(_ root: String, day: Int, size: UInt64, status: ScanStatus = .complete, id: UUID = UUID()) -> Scan {
        let started = Date(timeIntervalSince1970: TimeInterval(1_800_000_000 + day * 86_400))
        return Scan(
            id: id, rootPath: root, status: status, startedAt: started,
            finishedAt: status == .running ? nil : started, totalDiskSize: size,
            totalLogicalSize: size, fileCount: 1, dirCount: 1, errorCount: 0,
            unreadablePaths: [], progress: nil, totalPrivateSize: size, totalSharedSize: 0,
            failureReason: nil
        )
    }

    private func diff(a: Scan, b: Scan, grown: [DiffEntry], freed: [DiffEntry] = []) -> ScanDiff {
        ScanDiff(
            scanA: a.id, scanB: b.id, scanAStartedAt: a.startedAt, scanBStartedAt: b.startedAt,
            reversedChronology: nil, rootPath: a.rootPath, diskDelta: 3, logicalDelta: 3,
            fileCountDelta: 1, dirCountDelta: 0, errorCountDelta: 0, grown: grown, freed: freed
        )
    }

    private func key(_ a: Scan, _ b: Scan) -> String {
        "\(a.id.uuidString.lowercased())|\(b.id.uuidString.lowercased())"
    }

    func testPreviousCompleteScanIsTheNewestOlderCompleteScanOfTheSameRoot() {
        let target = scan("/Users/a", day: 10, size: 500)
        let older = scan("/Users/a/", day: 8, size: 400)          // trailing slash: same root
        let olderStill = scan("/Users/a", day: 2, size: 100)
        let newer = scan("/Users/a", day: 12, size: 600)           // after the target: not a baseline
        let cancelled = scan("/Users/a", day: 9, size: 450, status: .cancelled)
        let otherRoot = scan("/Users/b", day: 9, size: 450)
        let scans = [newer, target, cancelled, otherRoot, older, olderStill]
        XCTAssertEqual(ScansModel.previousCompleteScan(before: target, in: scans)?.id, older.id)
        XCTAssertNil(ScansModel.previousCompleteScan(before: olderStill, in: scans), "the root's first scan has no baseline")
    }

    func testDeltaMarkReadsTheGrownListAndSkipsTheRoot() {
        let a = scan("/r", day: 1, size: 1)
        let b = scan("/r", day: 2, size: 4)
        let d = diff(a: a, b: b, grown: [
            DiffEntry(path: "/r", before: 1, after: 4, delta: 3),
            DiffEntry(path: "/r/new", before: nil, after: 2, delta: 2),
            DiffEntry(path: "/r/old", before: 1, after: 2, delta: 1),
        ], freed: [DiffEntry(path: "/r/gone", before: 5, after: nil, delta: -5)])
        XCTAssertEqual(ScansModel.deltaMark(for: "/r/new", in: d), .new(bytes: 2))
        XCTAssertEqual(ScansModel.deltaMark(for: "/r/old", in: d), .grown(bytes: 1))
        XCTAssertNil(ScansModel.deltaMark(for: "/r/gone", in: d), "freed is not a growth mark")
        XCTAssertNil(ScansModel.deltaMark(for: "/r/other", in: d))
        // Through the model: the root itself is never marked.
        let mock = MockAPIClient()
        mock.scans = [b, a]
        mock.diffsByPair[key(a, b)] = d
        let model = ScansModel(client: mock, connectionState: .connecting)
        Task { await model.refresh() }
        // refresh() selects b (newest terminal) and loads the delta a → b.
        let exp = expectation(description: "delta loaded")
        Task {
            for _ in 0..<200 where model.delta == nil { try? await Task.sleep(nanoseconds: 5_000_000) }
            exp.fulfill()
        }
        wait(for: [exp], timeout: 5)
        XCTAssertEqual(model.delta?.scanA, a.id)
        XCTAssertEqual(model.deltaBaseline?.id, a.id)
        XCTAssertNil(model.deltaMark(for: "/r"), "the root grows whenever anything under it does")
        XCTAssertEqual(model.deltaMark(for: "/r/new"), .new(bytes: 2))
        XCTAssertEqual(mock.diffRequests.map { "\($0.a)|\($0.b)" }, ["\(a.id)|\(b.id)"], "older first, once")
    }

    func testFirstScanOfARootHasNoDeltaAndNoError() async {
        let mock = MockAPIClient()
        let only = scan("/r", day: 1, size: 1)
        mock.scans = [only]
        let model = ScansModel(client: mock, connectionState: .connecting)
        await model.refresh()
        XCTAssertNil(model.delta)
        XCTAssertNil(model.deltaBaseline)
        XCTAssertNil(model.lastError)
        XCTAssertTrue(mock.diffRequests.isEmpty, "nothing to diff against, nothing asked")
        XCTAssertEqual(model.comparableScans, [])
    }

    func testCompareAlwaysAsksOlderFirstWhicheverWasPicked() async throws {
        let mock = MockAPIClient()
        let a = scan("/r", day: 1, size: 1)
        let b = scan("/r", day: 5, size: 5)
        let c = scan("/r", day: 9, size: 9)
        mock.scans = [c, b, a]
        mock.diffsByPair[key(b, c)] = diff(a: b, b: c, grown: [])
        mock.diffsByPair[key(a, b)] = diff(a: a, b: b, grown: [])
        let model = ScansModel(client: mock, connectionState: .connecting)
        await model.refresh()                       // selects c; delta b → c
        await model.select(scanID: b.id)            // now the selection is the MIDDLE scan
        XCTAssertEqual(model.comparableScans.map(\.id), [c.id, a.id], "others of the root, newest first")
        mock.diffsByPair[key(b, c)] = diff(a: b, b: c, grown: [])
        model.compareWithID = c.id                  // picked a NEWER scan
        await model.loadComparison()
        XCTAssertEqual(mock.diffRequests.last.map { "\($0.a)|\($0.b)" }, "\(b.id)|\(c.id)", "selection is older: it goes first")
        model.compareWithID = a.id                  // picked an OLDER scan
        await model.loadComparison()
        XCTAssertEqual(mock.diffRequests.last.map { "\($0.a)|\($0.b)" }, "\(a.id)|\(b.id)", "the pick is older: it goes first")
        XCTAssertEqual(model.comparison?.scanA, a.id)
        model.compareWithID = nil
        await model.loadComparison()
        XCTAssertNil(model.comparison)
    }
}
