// The model's side of the growth notification (v1.2 Phase 2): fetches the
// category series and the summary through the one boundary, speaks at most
// once per scan id ever, and stays silent when disabled or when a fetch
// fails.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelGrowthAlertTests: XCTestCase {

    final class Recorder: GrowthAlertDelivering, @unchecked Sendable {
        private let lock = NSLock()
        private var _delivered: [(title: String, body: String, scanID: UUID)] = []
        var delivered: [(title: String, body: String, scanID: UUID)] { lock.lock(); defer { lock.unlock() }; return _delivered }
        func deliver(title: String, body: String, scanID: UUID) async {
            record(title: title, body: body, scanID: scanID)
        }
        private func record(title: String, body: String, scanID: UUID) {
            lock.lock(); defer { lock.unlock() }
            _delivered.append((title, body, scanID))
        }
    }

    final class MemoryStore: NotifiedScanStore, @unchecked Sendable {
        private let lock = NSLock()
        private var ids: Set<UUID> = []
        func contains(_ id: UUID) -> Bool { lock.lock(); defer { lock.unlock() }; return ids.contains(id) }
        func insert(_ id: UUID) { lock.lock(); defer { lock.unlock() }; ids.insert(id) }
    }

    private let t0 = Date(timeIntervalSince1970: 1_800_000_000)
    private func scan(_ bytes: UInt64, day: Double, root: String = "/Users/ghost") -> Scan {
        Scan(
            id: UUID(), rootPath: root, status: .complete, startedAt: t0.addingTimeInterval(day * 86_400),
            finishedAt: t0.addingTimeInterval(day * 86_400), totalDiskSize: bytes, totalLogicalSize: bytes,
            fileCount: 1, dirCount: 1, errorCount: 0, unreadablePaths: [], progress: nil
        )
    }

    private func makeModel(enabled: Bool = true) -> (ScansModel, MockAPIClient, Recorder, MemoryStore) {
        let mock = MockAPIClient()
        let prev = scan(100_000_000_000, day: 0)
        let cur = scan(120_000_000_000, day: 7)
        mock.scans = [cur, prev]
        mock.growthToReturn = Growth(
            rootPath: "/Users/ghost", groupBy: "category",
            points: [GrowthPoint(scanId: prev.id, startedAt: prev.startedAt, totalDiskSize: prev.totalDiskSize, totalPrivateSize: nil, fileCount: 1),
                     GrowthPoint(scanId: cur.id, startedAt: cur.startedAt, totalDiskSize: cur.totalDiskSize, totalPrivateSize: nil, fileCount: 1)],
            series: [GrowthLine(key: "toolManagedCache", values: [1_000_000_000, 9_000_000_000])], forecast: nil, note: ""
        )
        mock.hotspotsSummary = HotspotsSummary(groups: [], reclaimEstimate: 7_000_000_000, reviewDiskSize: 0, cloudDataloadedLogicalSize: 0, cloudDataloadedDiskSize: 0)
        let model = ScansModel(client: mock)
        model.scans = mock.scans
        model.growthAlertsEnabled = enabled
        let rec = Recorder()
        let store = MemoryStore()
        model.growthAlertDeliverer = rec
        model.notifiedScans = store
        return (model, mock, rec, store)
    }

    func testDeliversOnceWithTheCategorySeriesAndTheSummary() async throws {
        let (model, mock, rec, store) = makeModel()
        let cur = model.scans[0]
        let delivered = await model.notifyGrowthIfWarranted(for: cur)
        let alert = try XCTUnwrap(delivered)
        XCTAssertEqual(alert.kind, .grew)
        XCTAssertEqual(mock.growthRequests.last?.groupBy, "category", "the category breakdown names the culprit")
        XCTAssertEqual(rec.delivered.count, 1)
        XCTAssertEqual(rec.delivered[0].title, "Phantom: ghost grew 20 GB this week")
        XCTAssertEqual(rec.delivered[0].body, "8 GB of it tool-managed caches · safe to reclaim: 7 GB. Open the History pane.")
        XCTAssertTrue(store.contains(cur.id))
        // A second poll, a relaunch, whatever: the same scan never speaks twice.
        let again = await model.notifyGrowthIfWarranted(for: cur)
        XCTAssertNil(again)
        XCTAssertEqual(rec.delivered.count, 1)
    }

    func testSilentWhenDisabledWithoutAPreviousScanOrWhenTheRootShrank() async {
        let (model, _, rec, _) = makeModel(enabled: false)
        let disabled = await model.notifyGrowthIfWarranted(for: model.scans[0])
        XCTAssertNil(disabled)
        model.growthAlertsEnabled = true
        // The root's FIRST scan has nothing to compare against.
        let first = await model.notifyGrowthIfWarranted(for: model.scans[1])
        XCTAssertNil(first)
        // Shrank: silent even with everything enabled.
        let smaller = scan(50_000_000_000, day: 14)
        model.scans.insert(smaller, at: 0)
        let shrank = await model.notifyGrowthIfWarranted(for: smaller)
        XCTAssertNil(shrank)
        XCTAssertTrue(rec.delivered.isEmpty)
    }

    func testAFailedFetchIsSilenceNotAnErrorBanner() async {
        let (model, mock, rec, store) = makeModel()
        mock.failWith = .serverUnreachable("down")
        let cur = model.scans[0]
        let result = await model.notifyGrowthIfWarranted(for: cur)
        XCTAssertNil(result)
        XCTAssertTrue(rec.delivered.isEmpty)
        XCTAssertNil(model.lastError, "a notification is never worth an error banner")
        XCTAssertFalse(store.contains(cur.id), "not marked: a later poll may still speak once the server is back")
    }
}
