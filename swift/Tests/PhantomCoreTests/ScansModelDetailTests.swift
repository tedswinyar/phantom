// Detail-surface tests for ScansModel: scan selection, the paged file
// listing (cursor pagination with an exhausted-cursor stop), inspector entry
// selection (page cache before fetch), breadcrumbs, and the treemap
// viewport seam. Driven through MockAPIClient at the network boundary.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelDetailTests: XCTestCase {

    // MARK: refresh auto-select

    func testRefreshAutoSelectsNewestTerminalScanWhenNothingSelected() async {
        let mock = MockAPIClient()
        let running = makeScan(rootPath: "/newest-but-running", status: .running)
        let newestTerminal = makeScan(rootPath: "/newest-terminal")
        let older = makeScan(rootPath: "/older")
        mock.scans = [running, newestTerminal, older] // server order: newest first
        let model = ScansModel(client: mock)
        await model.refresh()
        XCTAssertEqual(
            model.selectedScanID, newestTerminal.id,
            "refresh opens on the newest TERMINAL scan — a running scan has no results yet"
        )
        XCTAssertNil(model.lastError)
    }

    func testRefreshKeepsAnExistingSelection() async {
        let mock = MockAPIClient()
        let newest = makeScan(rootPath: "/newest")
        let chosen = makeScan(rootPath: "/deliberately-older")
        mock.scans = [newest, chosen]
        let model = ScansModel(client: mock)
        await model.refresh()
        await model.select(scanID: chosen.id)
        await model.refresh()
        XCTAssertEqual(model.selectedScanID, chosen.id, "refresh never steals an explicit selection")
    }

    func testRefreshSelectsNothingWhenAllScansAreRunning() async {
        let mock = MockAPIClient()
        mock.scans = [makeScan(status: .running)]
        let model = ScansModel(client: mock)
        await model.refresh()
        XCTAssertNil(model.selectedScanID)
    }

    private func makeScan(
        id: UUID = UUID(),
        rootPath: String = "/Users/ghost/Code",
        status: ScanStatus = .complete
    ) -> Scan {
        Scan(
            id: id, rootPath: rootPath, status: status, startedAt: Date(),
            finishedAt: status == .running ? nil : Date(),
            totalDiskSize: 4096, totalLogicalSize: 8192,
            fileCount: 2, dirCount: 1, errorCount: 0, unreadablePaths: [],
            progress: status == .running
                ? Scan.Progress(filesSeen: 1, bytesSeen: 512, currentPath: rootPath)
                : nil
        )
    }

    private func makeEntry(path: String, diskSize: UInt64 = 1024) -> ScanEntry {
        ScanEntry(
            path: path, parentPath: "/Users/ghost/Code",
            name: String(path.split(separator: "/").last ?? "?"), isDir: false,
            diskSize: diskSize, logicalSize: diskSize * 2, modifiedAt: nil,
            fileType: "rs", category: nil, nlink: 1, dev: 1, ino: 1
        )
    }

    private func makeModel(
        _ mock: MockAPIClient, scan: Scan
    ) async -> ScansModel {
        mock.scans = [scan]
        let model = ScansModel(client: mock)
        await model.refresh()
        return model
    }

    // MARK: - select(scanID:)

    func testSelectTerminalScanLoadsTypesAndFirstFilesPage() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let page1 = [makeEntry(path: "/Users/ghost/Code/a.rs")]
        mock.filePages = [nil: FilePage(files: page1, nextCursor: "100")]
        mock.typeTotals = [FileTypeTotal(fileType: "rs", diskSize: 1024, fileCount: 1)]
        let model = await makeModel(mock, scan: scan)

        await model.select(scanID: scan.id)
        XCTAssertEqual(model.selectedScan?.id, scan.id)
        XCTAssertEqual(model.files, page1)
        XCTAssertEqual(model.filesNextCursor, "100")
        XCTAssertEqual(model.typeTotals, mock.typeTotals)
        XCTAssertNil(model.lastError)
    }

    func testSelectRunningScanStartsPollingInsteadOfLoading() async throws {
        let mock = MockAPIClient()
        let running = makeScan(status: .running)
        var done = running
        done.status = .complete
        done.progress = nil
        mock.scanUpdates[running.id] = [done]
        let model = await makeModel(mock, scan: running)

        await model.select(scanID: running.id)
        XCTAssertTrue(model.files.isEmpty, "a running scan has no file results to load")
        await model.awaitPolling(id: running.id)
        XCTAssertEqual(model.scans.first?.status, .complete)
        XCTAssertNil(model.lastError)
    }

    func testSelectionChangeResetsPerScanState() async {
        let mock = MockAPIClient()
        let scanA = makeScan(rootPath: "/a")
        let scanB = makeScan(rootPath: "/b")
        mock.scans = [scanA, scanB]
        mock.filePages = [nil: FilePage(files: [makeEntry(path: "/a/x.rs")], nextCursor: nil)]
        mock.treemapsByRoot = [nil: TreemapLayout(rootPath: "/a", totalSize: 1, rects: [])]
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.select(scanID: scanA.id)
        await model.loadTreemap(scanID: scanA.id, width: 800, height: 600)
        await model.selectEntry(path: "/a/x.rs")
        XCTAssertFalse(model.files.isEmpty)
        XCTAssertNotNil(model.treemap)
        XCTAssertNotNil(model.selectedEntry)

        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        await model.select(scanID: scanB.id)
        XCTAssertTrue(model.files.isEmpty)
        XCTAssertNil(model.treemap, "treemap is per-scan state")
        XCTAssertTrue(model.treemapRootStack.isEmpty)
        XCTAssertNil(model.selectedEntry)
        XCTAssertNil(model.selectedEntryPath)
        XCTAssertNil(model.fileTypeFilter)
    }

    // MARK: - files paging

    func testLoadMoreFilesAppendsNextPageAndStopsWhenExhausted() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let page1 = [makeEntry(path: "/Users/ghost/Code/a.rs")]
        let page2 = [makeEntry(path: "/Users/ghost/Code/b.rs")]
        mock.filePages = [
            nil: FilePage(files: page1, nextCursor: "100"),
            "100": FilePage(files: page2, nextCursor: nil),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id) // loads page 1

        await model.loadMoreFiles()
        XCTAssertEqual(model.files, page1 + page2, "pages accumulate, newest appended")
        XCTAssertNil(model.filesNextCursor, "no header on the last page == complete")
        XCTAssertEqual(mock.fileCursorsRequested, [nil, "100"])

        // Exhausted cursor: calling again must NOT hit the client (a refetch
        // of page one would silently duplicate rows).
        await model.loadMoreFiles()
        XCTAssertEqual(mock.fileCursorsRequested, [nil, "100"], "no request after the last page")
        XCTAssertEqual(model.files, page1 + page2)
        XCTAssertNil(model.lastError)
    }

    func testLoadFilesAppliesNormalizedFilterAndSearch() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        model.fileTypeFilter = "rs"
        model.fileSearch = "  main  "
        await model.loadFiles()
        XCTAssertEqual(mock.lastFilesFileType, "rs")
        XCTAssertEqual(mock.lastFilesSearch, "main", "search is trimmed before it hits the wire")

        model.fileTypeFilter = nil
        model.fileSearch = "   "
        await model.loadFiles()
        XCTAssertNil(mock.lastFilesFileType)
        XCTAssertNil(mock.lastFilesSearch, "whitespace-only search means no search param")
    }

    func testLoadMoreFilesErrorSurfacesAndKeepsLoadedPages() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let page1 = [makeEntry(path: "/Users/ghost/Code/a.rs")]
        // Cursor "100" is advertised but not served → 400 from the mock.
        mock.filePages = [nil: FilePage(files: page1, nextCursor: "100")]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.loadMoreFiles()
        XCTAssertNotNil(model.lastError)
        XCTAssertEqual(model.files, page1, "a failed page load must not clobber loaded rows")
    }

    // MARK: - entry selection (inspector)

    func testSelectEntryServesFromTheLoadedPageWithoutFetching() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let cached = makeEntry(path: "/Users/ghost/Code/a.rs")
        mock.filePages = [nil: FilePage(files: [cached], nextCursor: nil)]
        // entriesByPath deliberately EMPTY: a fetch would 404.
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.selectEntry(path: cached.path)
        XCTAssertEqual(model.selectedEntry, cached)
        XCTAssertEqual(model.selectedEntryPath, cached.path)
        XCTAssertEqual(mock.getEntryCallCount, 0, "page rows are the cache; no fetch")
        XCTAssertNil(model.lastError)
    }

    func testSelectEntryFetchesDirectoriesAndOffPageRows() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        let dir = ScanEntry(
            path: "/Users/ghost/Code/sub", parentPath: "/Users/ghost/Code",
            name: "sub", isDir: true, diskSize: 2048, logicalSize: 2048,
            modifiedAt: nil, fileType: nil, category: nil, nlink: 3, dev: 1, ino: 2
        )
        mock.entriesByPath[dir.path] = dir
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.selectEntry(path: dir.path)
        XCTAssertEqual(model.selectedEntry, dir)
        XCTAssertEqual(mock.getEntryCallCount, 1)
    }

    func testSelectEntryFailureClearsEntryAndSurfacesError() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.selectEntry(path: "/Users/ghost/Code/missing")
        XCTAssertNil(model.selectedEntry)
        XCTAssertNotNil(model.lastError)
    }

    func testSelectEntryNilClearsSelection() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let cached = makeEntry(path: "/Users/ghost/Code/a.rs")
        mock.filePages = [nil: FilePage(files: [cached], nextCursor: nil)]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.selectEntry(path: cached.path)

        await model.selectEntry(path: nil)
        XCTAssertNil(model.selectedEntry)
        XCTAssertNil(model.selectedEntryPath)
    }

    // MARK: - hotspots (reclaimable space)

    private func makeSummary() -> HotspotsSummary {
        HotspotsSummary(
            groups: [HotspotGroup(
                ruleId: "cargo-target", label: "Rust target/ directories",
                category: "staleProjectArtifact",
                hint: "`cargo clean` or delete; the next `cargo build` regenerates it",
                diskSize: 17_179_869_184, listedDiskSize: 17_179_869_184,
                logicalSize: 18_179_869_184, fileCount: 5120,
                topPaths: ["/Users/ghost/Code/dormant/target"])],
            reclaimEstimate: 17_179_869_184, reviewDiskSize: 0,
            cloudDataloadedLogicalSize: 0, cloudDataloadedDiskSize: 0
        )
    }

    func testSelectTerminalScanLoadsHotspots() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        mock.hotspotsSummary = makeSummary()
        let model = await makeModel(mock, scan: scan)

        await model.select(scanID: scan.id)
        XCTAssertEqual(model.hotspots, mock.hotspotsSummary)
        XCTAssertFalse(model.hotspotsStillRunning)
        XCTAssertNil(model.lastError)
    }

    func testSelectTerminalScanWithNothingClassifiedIsHonestlyEmpty() async throws {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        // hotspotsSummary left nil → the mock serves the all-empty summary,
        // like the API does for a terminal scan with nothing stored.
        let model = await makeModel(mock, scan: scan)

        await model.select(scanID: scan.id)
        let summary = try XCTUnwrap(model.hotspots, "empty is a LOADED state, not a failure")
        XCTAssertTrue(summary.isEmpty)
        XCTAssertNil(model.lastError)
    }

    func testLoadHotspotsWhileRunningSetsPendingStateNotError() async {
        let mock = MockAPIClient()
        mock.hotspotsConflict = true
        let model = ScansModel(client: mock)
        await model.loadHotspots(scanID: UUID())
        XCTAssertTrue(model.hotspotsStillRunning)
        XCTAssertNil(model.hotspots)
        XCTAssertNil(model.lastError, "409-while-running is an expected state, not an error")
    }

    func testLoadHotspotsErrorSurfaces() async {
        let mock = MockAPIClient()
        mock.failWith = .decodingFailed("garbage bytes")
        let model = ScansModel(client: mock)
        await model.loadHotspots(scanID: UUID())
        XCTAssertNotNil(model.lastError)
        XCTAssertNil(model.hotspots)
        XCTAssertFalse(model.hotspotsStillRunning)
    }

    func testSelectionChangeResetsHotspots() async {
        let mock = MockAPIClient()
        let scanA = makeScan(rootPath: "/a")
        let runningB = makeScan(rootPath: "/b", status: .running)
        mock.scans = [scanA, runningB]
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        mock.hotspotsSummary = makeSummary()
        // Selecting a running scan starts polling; an empty queue makes the
        // first poll error out immediately so the spawned task terminates.
        mock.scanUpdates[runningB.id] = []
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.select(scanID: scanA.id)
        XCTAssertNotNil(model.hotspots)

        // Selecting the RUNNING scan resets and does not load results.
        await model.select(scanID: runningB.id)
        XCTAssertNil(model.hotspots, "hotspots are per-scan state")
        XCTAssertFalse(model.hotspotsStillRunning)
        await model.awaitPolling(id: runningB.id)
    }

    // MARK: - breadcrumbs

    func testBreadcrumbsComposeScanRootPlusDrillStack() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        let sub = "/Users/ghost/Code/sub"
        mock.treemapsByRoot = [
            nil: TreemapLayout(rootPath: scan.rootPath, totalSize: 3, rects: []),
            sub: TreemapLayout(rootPath: sub, totalSize: 2, rects: []),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)
        XCTAssertEqual(model.breadcrumbs, [scan.rootPath])

        await model.drillIn(to: sub)
        XCTAssertEqual(model.breadcrumbs, [scan.rootPath, sub])
    }

    func testNavigateToCrumbJumpsToThatLevel() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        let sub = "/Users/ghost/Code/sub"
        let deep = "/Users/ghost/Code/sub/deep"
        mock.treemapsByRoot = [
            nil: TreemapLayout(rootPath: scan.rootPath, totalSize: 3, rects: []),
            sub: TreemapLayout(rootPath: sub, totalSize: 2, rects: []),
            deep: TreemapLayout(rootPath: deep, totalSize: 1, rects: []),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)
        await model.drillIn(to: sub)
        await model.drillIn(to: deep)
        XCTAssertEqual(model.breadcrumbs, [scan.rootPath, sub, deep])

        // The LAST crumb is where we already are: no refetch.
        let requestsBefore = mock.treemapRootsRequested.count
        await model.navigate(toCrumb: 2)
        XCTAssertEqual(mock.treemapRootsRequested.count, requestsBefore)

        // Jump to the middle crumb, then the scan root.
        await model.navigate(toCrumb: 1)
        XCTAssertEqual(model.breadcrumbs, [scan.rootPath, sub])
        XCTAssertEqual(model.treemap?.rootPath, sub)
        await model.navigate(toCrumb: 0)
        XCTAssertEqual(model.breadcrumbs, [scan.rootPath])
        XCTAssertEqual(model.treemap?.rootPath, scan.rootPath)
        XCTAssertNil(model.lastError)
    }

    // MARK: - viewport seam

    func testViewportUpdateAffectsNextFetchWithoutFetchingNow() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.filePages = [nil: FilePage(files: [], nextCursor: nil)]
        let sub = "/Users/ghost/Code/sub"
        mock.treemapsByRoot = [
            nil: TreemapLayout(rootPath: scan.rootPath, totalSize: 3, rects: []),
            sub: TreemapLayout(rootPath: sub, totalSize: 2, rects: []),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)
        XCTAssertEqual(mock.treemapRootsRequested.count, 1)

        // Resize: no request now (the view re-scales the fetched layout)…
        model.updateTreemapViewport(width: 1024, height: 768)
        XCTAssertEqual(mock.treemapRootsRequested.count, 1, "resize must not refetch")

        // …but the next navigation lays out at the NEW size.
        await model.drillIn(to: sub)
        XCTAssertEqual(mock.lastTreemapWidth, 1024)
        XCTAssertEqual(mock.lastTreemapHeight, 768)
    }
}
