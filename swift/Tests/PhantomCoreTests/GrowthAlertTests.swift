// Every sentence the growth notification can say, and every silence
// (v1.2 Phase 2, phantom-adq.2).

import XCTest
@testable import PhantomCore

final class GrowthAlertTests: XCTestCase {

    private let t0 = Date(timeIntervalSince1970: 1_800_000_000)
    private func size(_ b: UInt64) -> String { ScansModel.sizeText(b) }

    private func scan(_ bytes: UInt64, daysAfter: Double = 0, root: String = "/Users/ghost") -> Scan {
        Scan(
            id: UUID(), rootPath: root, status: .complete, startedAt: t0.addingTimeInterval(daysAfter * 86_400),
            finishedAt: t0.addingTimeInterval(daysAfter * 86_400), totalDiskSize: bytes, totalLogicalSize: bytes,
            fileCount: 1, dirCount: 1, errorCount: 0, unreadablePaths: [], progress: nil
        )
    }

    private func categorySeries(_ lines: [(String, [UInt64?])], forecast: GrowthForecast? = nil) -> Growth {
        Growth(
            rootPath: "/Users/ghost", groupBy: "category",
            points: [
                GrowthPoint(scanId: UUID(), startedAt: t0, totalDiskSize: 100, totalPrivateSize: 100, fileCount: 1),
                GrowthPoint(scanId: UUID(), startedAt: t0.addingTimeInterval(86_400), totalDiskSize: 120, totalPrivateSize: 120, fileCount: 1),
            ],
            series: lines.map { GrowthLine(key: $0.0, values: $0.1) },
            forecast: forecast, note: ""
        )
    }

    private func summary(reclaim: UInt64) -> HotspotsSummary {
        HotspotsSummary(groups: [], reclaimEstimate: reclaim, reviewDiskSize: 0, cloudDataloadedLogicalSize: 0, cloudDataloadedDiskSize: 0)
    }

    func testGrowthPastTheByteFloorSpeaksNamingTheTopCategoryAndTheReclaim() throws {
        let prev = scan(100_000_000_000)
        let cur = scan(120_300_000_000, daysAfter: 7)
        let growth = categorySeries([
            ("regenerableArtifact", [10_000_000_000, 24_100_000_000]),   // +14.1 GB — the headline
            ("cache", [5_000_000_000, 8_000_000_000]),                   // +3 GB
            ("other", [1_000_000_000, 90_000_000_000]),                  // never the headline
        ])
        let alert = try XCTUnwrap(GrowthAlert.evaluate(previous: prev, current: cur, growth: growth, hotspots: summary(reclaim: 12_900_000_000)))
        XCTAssertEqual(alert.kind, .grew)
        XCTAssertEqual(alert.grewBytes, 20_300_000_000)
        XCTAssertEqual(alert.topCategory?.key, "regenerableArtifact")
        XCTAssertEqual(alert.topCategory?.bytes, 14_100_000_000)
        XCTAssertEqual(alert.title(size: size), "Phantom: ghost grew 20.3 GB this week")
        XCTAssertEqual(alert.body(size: size), "14.1 GB of it build artifacts · safe to reclaim: 12.9 GB. Open the History pane.")
    }

    func testShrinkageAndFlatAreSilentWhateverTheSeriesSays() {
        let growth = categorySeries([("cache", [1, 1_000_000_000_000])])
        XCTAssertNil(GrowthAlert.evaluate(previous: scan(100), current: scan(90, daysAfter: 1), growth: growth, hotspots: nil))
        XCTAssertNil(GrowthAlert.evaluate(previous: scan(100), current: scan(100, daysAfter: 1), growth: growth, hotspots: nil))
    }

    func testBelowBothFloorsIsSilentAndEitherFloorSpeaks() {
        // 4 GB on 200 GB = 2 %: below 5 GB and below 5 %.
        XCTAssertNil(GrowthAlert.evaluate(previous: scan(200_000_000_000), current: scan(204_000_000_000, daysAfter: 1), growth: nil, hotspots: nil))
        // 4 GB on 40 GB = 10 %: the percent floor speaks.
        XCTAssertEqual(GrowthAlert.evaluate(previous: scan(40_000_000_000), current: scan(44_000_000_000, daysAfter: 1), growth: nil, hotspots: nil)?.kind, .grew)
        // 6 GB on 1 TB = 0.6 %: the byte floor speaks.
        XCTAssertEqual(GrowthAlert.evaluate(previous: scan(1_000_000_000_000), current: scan(1_006_000_000_000, daysAfter: 1), growth: nil, hotspots: nil)?.kind, .grew)
        // Custom thresholds are honoured.
        let strict = GrowthAlertThresholds(minBytes: 1_000_000_000, minPercent: 1)
        XCTAssertEqual(GrowthAlert.evaluate(previous: scan(200_000_000_000), current: scan(204_000_000_000, daysAfter: 1), growth: nil, hotspots: nil, thresholds: strict)?.kind, .grew)
    }

    func testAForecastUnderThirtyDaysIsTheRarerAlertWithTheCaveat() throws {
        let forecast = GrowthForecast(method: "linear", pointsUsed: 3, spanDays: 6, bytesPerDay: 10_000_000_000, latestBytes: 120, availableBytes: 100_000_000_000, daysUntilFull: 12.4, projectedFullAt: t0, caveat: "Linear fit — assumes the rate continues.")
        let growth = categorySeries([("cache", [1, 2])], forecast: forecast)
        // Growth itself is below the floors (1 GB, 0.5 %) so this is the forecast branch.
        let alert = try XCTUnwrap(GrowthAlert.evaluate(previous: scan(200_000_000_000), current: scan(201_000_000_000, daysAfter: 1), growth: growth, hotspots: nil))
        XCTAssertEqual(alert.kind, .forecastFull)
        XCTAssertEqual(alert.title(size: size), "Phantom: ghost may be full in 12 days")
        XCTAssertEqual(alert.body(size: size), "1 B of it caches. At the recent rate. Linear fit — assumes the rate continues.")
        // Not growing, or too far out: silent.
        let flat = GrowthForecast(method: "linear", pointsUsed: 3, spanDays: 6, bytesPerDay: 0, latestBytes: 120, availableBytes: 1, daysUntilFull: nil, projectedFullAt: nil, caveat: "c")
        XCTAssertNil(GrowthAlert.evaluate(previous: scan(200_000_000_000), current: scan(201_000_000_000, daysAfter: 1), growth: categorySeries([], forecast: flat), hotspots: nil))
        let far = GrowthForecast(method: "linear", pointsUsed: 3, spanDays: 6, bytesPerDay: 1, latestBytes: 120, availableBytes: 1, daysUntilFull: 400, projectedFullAt: t0, caveat: "c")
        XCTAssertNil(GrowthAlert.evaluate(previous: scan(200_000_000_000), current: scan(201_000_000_000, daysAfter: 1), growth: categorySeries([], forecast: far), hotspots: nil))
    }

    func testTopCategoryNeedsACategorySeriesAndARise() {
        var g = categorySeries([("cache", [10, 5]), ("regenerableArtifact", [nil, 7])])
        XCTAssertNil(GrowthAlert.topCategory(in: g), "a fall and a null-then-value are not rises")
        g = Growth(rootPath: "/r", groupBy: "topLevelDir", points: g.points, series: [GrowthLine(key: "Code", values: [1, 9])], forecast: nil, note: "")
        XCTAssertNil(GrowthAlert.topCategory(in: g), "only the category breakdown names a category")
        XCTAssertNil(GrowthAlert.topCategory(in: nil))
        // Ties break by key so the answer is stable.
        g = categorySeries([("cache", [0, 5]), ("modelCache", [0, 5])])
        XCTAssertEqual(GrowthAlert.topCategory(in: g)?.key, "cache")
    }

    func testPeriodPhraseAndRootShortening() {
        XCTAssertEqual(GrowthAlert.periodPhrase(0.7), "since yesterday")
        XCTAssertEqual(GrowthAlert.periodPhrase(6.9), "this week")
        XCTAssertEqual(GrowthAlert.periodPhrase(16.2), "in 16 days")
        XCTAssertEqual(GrowthAlert.periodPhrase(90), "since 90 days ago")
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        XCTAssertEqual(GrowthAlert.shortRoot(home), "~")
        XCTAssertEqual(GrowthAlert.shortRoot(home + "/"), "~")
        XCTAssertEqual(GrowthAlert.shortRoot("/Volumes/Data/Projects/"), "Projects")
        XCTAssertEqual(GrowthAlert.shortRoot("/"), "/")
    }

    func testSizeTextIsDecimalLikeTheApp() {
        XCTAssertEqual(ScansModel.sizeText(999), "999 B")
        XCTAssertEqual(ScansModel.sizeText(20_300_000_000), "20.3 GB")
        XCTAssertEqual(ScansModel.sizeText(256_200_000_000), "256 GB")
        XCTAssertEqual(ScansModel.sizeText(2_000_000), "2 MB")
        XCTAssertEqual(ScansModel.sizeText(1_500), "1.5 KB")
    }

    func testDefaultsStoreRemembersAcrossInstancesAndCaps() {
        let suite = "GrowthAlertTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let id = UUID()
        let a = DefaultsNotifiedScanStore(defaults: defaults)
        XCTAssertFalse(a.contains(id))
        a.insert(id)
        a.insert(id)
        XCTAssertTrue(DefaultsNotifiedScanStore(defaults: defaults).contains(id), "a relaunch sees it")
        XCTAssertEqual(defaults.stringArray(forKey: DefaultsNotifiedScanStore.key)?.count, 1, "no duplicates")
        for _ in 0..<(DefaultsNotifiedScanStore.cap + 5) { a.insert(UUID()) }
        XCTAssertEqual(defaults.stringArray(forKey: DefaultsNotifiedScanStore.key)?.count, DefaultsNotifiedScanStore.cap)
        XCTAssertFalse(a.contains(id), "the oldest fell off the cap")
    }
}
