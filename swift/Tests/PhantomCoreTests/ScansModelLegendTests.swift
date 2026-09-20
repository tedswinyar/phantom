// Legend + paging model tests (folders-tree-spec.md SHOULDs): the per-scan
// type→slot assignment (one mapping for tiles, tree swatches, and chips),
// the chip toggle's highlight+filter coupling, and the scroll-triggered
// paging seam. Driven through MockAPIClient at the network boundary.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelLegendTests: XCTestCase {

    private let root = "/Users/ghost/Code"

    private func makeScan(fileCount: UInt64 = 100) -> Scan {
        Scan(
            id: UUID(), rootPath: root, status: .complete,
            startedAt: Date(), finishedAt: Date(),
            totalDiskSize: 1000, totalLogicalSize: 1200,
            fileCount: fileCount, dirCount: 5, errorCount: 0, unreadablePaths: [], progress: nil
        )
    }

    private func entry(_ path: String, disk: UInt64 = 1) -> ScanEntry {
        ScanEntry(
            path: path, parentPath: root,
            name: (path as NSString).lastPathComponent, isDir: false,
            diskSize: disk, logicalSize: disk, modifiedAt: nil,
            fileType: "rs", category: nil, nlink: 1, dev: 1, ino: 1
        )
    }

    /// Twelve types, biggest first, mirroring the server's sort. Type "t10"
    /// onward ranks past the nine slots; a nil (no-extension) bucket sits
    /// inside the top nine.
    private func totals() -> [FileTypeTotal] {
        var totals: [FileTypeTotal] = []
        for rank in 0..<12 {
            let type: String? = rank == 4 ? nil : "t\(rank)"
            totals.append(FileTypeTotal(
                fileType: type, diskSize: UInt64(1000 - rank * 50), fileCount: 10))
        }
        return totals
    }

    private func makeModel(_ mock: MockAPIClient, scan: Scan) async -> ScansModel {
        mock.scans = [scan]
        mock.treeChildren[root] = []
        let model = ScansModel(client: mock)
        await model.refresh()
        return model
    }

    // MARK: - slot assignment

    func testTopNineTypesTakeSlotsInServerOrder() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.typeTotals = totals()
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        // Slot order IS typeTotals order: the biggest type wears slot 0.
        XCTAssertEqual(model.legendSlots.count, 9)
        XCTAssertEqual(model.legendSlot(for: "t0"), 0)
        XCTAssertEqual(model.legendSlot(for: "t3"), 3)
        // The no-extension bucket earns a real slot when it ranks top-nine.
        XCTAssertEqual(model.legendSlot(for: nil), 4)
        XCTAssertEqual(model.legendSlot(for: "t8"), 8)
        // Rank ten and beyond — and unknown types — are "other".
        XCTAssertNil(model.legendSlot(for: "t10"))
        XCTAssertNil(model.legendSlot(for: "t11"))
        XCTAssertNil(model.legendSlot(for: "zzz"))
    }

    func testSlotAssignmentIsStableWithinAScanAndRebuildsPerScan() async {
        let mock = MockAPIClient()
        let scanA = makeScan()
        let scanB = makeScan()
        mock.scans = [scanA, scanB]
        mock.treeChildren[root] = []
        mock.typeTotals = totals()
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.select(scanID: scanA.id)
        let before = model.legendSlot(for: "t2")
        XCTAssertEqual(before, 2)
        // Repeated lookups inside one scan never move (the mapping derives
        // from typeTotals, which only changes on selection).
        XCTAssertEqual(model.legendSlot(for: "t2"), before)

        // Scan B ranks differently: colors change BY DESIGN.
        mock.typeTotals = [
            FileTypeTotal(fileType: "mp4", diskSize: 900, fileCount: 1),
            FileTypeTotal(fileType: "t2", diskSize: 800, fileCount: 1),
        ]
        await model.select(scanID: scanB.id)
        XCTAssertEqual(model.legendSlot(for: "mp4"), 0)
        XCTAssertEqual(model.legendSlot(for: "t2"), 1, "per-scan re-assignment")
        XCTAssertNil(model.legendSlot(for: "t0"))
    }

    // MARK: - chip toggle

    func testChipClickHighlightsAndFiltersThenSecondClickClears() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.typeTotals = totals()
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        let callsAfterSelect = mock.fileCursorsRequested.count

        await model.toggleLegendChip("t0")
        XCTAssertEqual(model.legendSelection, .type("t0"))
        XCTAssertEqual(model.fileTypeFilter, "t0")
        XCTAssertEqual(mock.lastFilesFileType, "t0", "the filter reached the wire")
        XCTAssertEqual(mock.fileCursorsRequested.count, callsAfterSelect + 1)

        await model.toggleLegendChip("t0")
        XCTAssertEqual(model.legendSelection, .none)
        XCTAssertNil(model.fileTypeFilter)
        XCTAssertNil(mock.lastFilesFileType, "cleared filter reloaded unfiltered")
    }

    func testChipSwitchMovesHighlightAndFilterTogether() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.typeTotals = totals()
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.toggleLegendChip("t0")
        await model.toggleLegendChip("t1")
        XCTAssertEqual(model.legendSelection, .type("t1"))
        XCTAssertEqual(model.fileTypeFilter, "t1")
    }

    func testNoExtensionChipHighlightsWithoutFiltering() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.typeTotals = totals()
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        let calls = mock.fileCursorsRequested.count

        // The files endpoint cannot express "extensionless", so the nil
        // chip highlights the map but must NOT touch the filter.
        await model.toggleLegendChip(nil)
        XCTAssertEqual(model.legendSelection, .type(nil))
        XCTAssertNil(model.fileTypeFilter)
        XCTAssertEqual(mock.fileCursorsRequested.count, calls, "no reload for a highlight-only chip")
    }

    func testLegendSelectionResetsOnScanChange() async {
        let mock = MockAPIClient()
        let scanA = makeScan()
        let scanB = makeScan()
        mock.scans = [scanA, scanB]
        mock.treeChildren[root] = []
        mock.typeTotals = totals()
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.select(scanID: scanA.id)
        await model.toggleLegendChip("t0")
        XCTAssertEqual(model.legendSelection, .type("t0"))

        await model.select(scanID: scanB.id)
        XCTAssertEqual(model.legendSelection, .none, "highlight is per-scan state")
        XCTAssertNil(model.fileTypeFilter)
    }

    // MARK: - scroll-triggered paging

    func testLastRowAppearingFetchesTheNextPage() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let page1 = [entry("\(root)/a.rs"), entry("\(root)/b.rs")]
        let page2 = [entry("\(root)/c.rs")]
        mock.filePages = [
            nil: FilePage(files: page1, nextCursor: "2"),
            "2": FilePage(files: page2, nextCursor: nil),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        // A mid-list row scrolling in does nothing.
        await model.fileRowAppeared(path: "\(root)/a.rs")
        XCTAssertEqual(model.files.count, 2)

        // The LAST row scrolling in fetches the next page.
        await model.fileRowAppeared(path: "\(root)/b.rs")
        XCTAssertEqual(model.files, page1 + page2)
        XCTAssertNil(model.filesNextCursor)

        // Bottom of a COMPLETE listing: no request, no error.
        let calls = mock.fileCursorsRequested.count
        await model.fileRowAppeared(path: "\(root)/c.rs")
        XCTAssertEqual(mock.fileCursorsRequested.count, calls)
        XCTAssertNil(model.lastError)
    }

    // MARK: - summary denominator

    func testFilesTotalCountOnlyForUnfilteredListings() async {
        let mock = MockAPIClient()
        let scan = makeScan(fileCount: 9301)
        mock.typeTotals = totals()
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        XCTAssertEqual(model.filesTotalCount, 9301, "unfiltered: the scan's full-walk count")
        model.fileSearch = "main"
        XCTAssertNil(model.filesTotalCount, "a search changes the population; no total exists")
        model.fileSearch = "  "
        XCTAssertEqual(model.filesTotalCount, 9301, "whitespace-only search is no search")
        model.fileTypeFilter = "rs"
        XCTAssertNil(model.filesTotalCount, "a type filter changes the population")
    }
}
