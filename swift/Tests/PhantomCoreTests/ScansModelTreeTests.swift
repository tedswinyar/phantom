// Folders-tree model tests (folders-tree-spec.md MUSTs): the lazy children
// cache (first-disclosure fetch, coalesced in-flight loads, per-scan reset,
// the ~4-fetch concurrency cap), the ancestor-chain and residual-row pure
// helpers, treemap→tree reveal, and double-click re-root stack rebuilds.
// Driven through MockAPIClient at the network boundary; the concurrency cap
// is asserted with the mock's parking gate — no sleeps anywhere.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelTreeTests: XCTestCase {

    private let root = "/Users/ghost/Code"

    private func makeScan(rootPath: String? = nil) -> Scan {
        Scan(
            id: UUID(), rootPath: rootPath ?? root, status: .complete,
            startedAt: Date(), finishedAt: Date(),
            totalDiskSize: 100, totalLogicalSize: 120,
            fileCount: 3, dirCount: 2, errorCount: 0, unreadablePaths: [], progress: nil
        )
    }

    private func entry(_ path: String, dir: Bool = false, disk: UInt64) -> ScanEntry {
        ScanEntry(
            path: path, parentPath: (path as NSString).deletingLastPathComponent,
            name: (path as NSString).lastPathComponent, isDir: dir,
            diskSize: disk, logicalSize: disk, modifiedAt: nil,
            fileType: dir ? nil : "rs", category: nil, nlink: 1, dev: 1, ino: 1
        )
    }

    private func makeModel(_ mock: MockAPIClient, scan: Scan) async -> ScansModel {
        mock.scans = [scan]
        let model = ScansModel(client: mock)
        await model.refresh()
        return model
    }

    // MARK: - lazy children cache

    func testSelectLoadsRootChildrenSortedByDiskSizeDescending() async throws {
        let mock = MockAPIClient()
        let scan = makeScan()
        // Server order is ORDER BY path; the model must re-sort disk-desc.
        mock.treeChildren[root] = [
            entry("\(root)/aaa.rs", disk: 10),
            entry("\(root)/sub", dir: true, disk: 80),
            entry("\(root)/zzz.rs", disk: 30),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        guard case .loaded(let children) = model.childrenByPath[root] else {
            return XCTFail("root children must be loaded on select, got \(String(describing: model.childrenByPath[root]))")
        }
        XCTAssertEqual(children.map(\.name), ["sub", "zzz.rs", "aaa.rs"],
                       "diskSize descending, not the server's path order")
    }

    func testLoadChildrenFetchesOncePerPath() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.treeChildren[root] = []
        let sub = "\(root)/sub"
        mock.treeChildren[sub] = [entry("\(sub)/x.rs", disk: 5)]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        let after = mock.getTreeCallCount

        await model.loadChildren(path: sub)
        await model.loadChildren(path: sub) // second disclosure: cache hit
        XCTAssertEqual(mock.getTreeCallCount, after + 1, "one GET per first disclosure")
    }

    func testLoadChildrenFailureIsRetryable() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.treeChildren[root] = []
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        let missing = "\(root)/ghostly"
        await model.loadChildren(path: missing) // mock 404s unknown paths
        guard case .failed = model.childrenByPath[missing] else {
            return XCTFail("expected .failed, got \(String(describing: model.childrenByPath[missing]))")
        }

        // The retry row's action: serve the path now, retry, loaded.
        mock.treeChildren[missing] = [entry("\(missing)/found.rs", disk: 1)]
        await model.retryChildren(path: missing)
        guard case .loaded(let children) = model.childrenByPath[missing] else {
            return XCTFail("retry must reload")
        }
        XCTAssertEqual(children.count, 1)
    }

    func testChildrenCacheResetsOnScanChange() async {
        let mock = MockAPIClient()
        let scanA = makeScan(rootPath: "/a")
        let scanB = makeScan(rootPath: "/b")
        mock.scans = [scanA, scanB]
        mock.treeChildren["/a"] = [entry("/a/x.rs", disk: 1)]
        mock.treeChildren["/b"] = [entry("/b/y.rs", disk: 2)]
        let model = ScansModel(client: mock)
        await model.refresh()

        await model.select(scanID: scanA.id)
        XCTAssertNotNil(model.childrenByPath["/a"])

        await model.select(scanID: scanB.id)
        XCTAssertNil(model.childrenByPath["/a"], "children cache is per-scan state")
        XCTAssertNotNil(model.childrenByPath["/b"], "new scan's root loads on select")
    }

    func testTreeFetchConcurrencyIsCappedAtFour() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.treeChildren[root] = []
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        // Park every fetch; request 10 directories at once (expand-all).
        mock.treeGateEnabled = true
        let paths = (0..<10).map { "\(root)/dir\($0)" }
        for path in paths { mock.treeChildren[path] = [] }
        let tasks = paths.map { path in
            Task { await model.loadChildren(path: path) }
        }

        // Wait (yielding, no sleeps) until the capped number are parked in
        // the mock — the other six must be queued in the model.
        for _ in 0..<10_000 where mock.parkedTreeFetches < 4 {
            await Task.yield()
        }
        XCTAssertEqual(mock.parkedTreeFetches, 4, "exactly the cap in flight")
        XCTAssertEqual(mock.treeInFlightPeak, 4)

        // Drain: stop gating NEW calls, then keep releasing anything parked
        // (a call that read the gate flag before the flip can still park
        // once) until every load reaches a terminal state. Bounded, no
        // sleeps — a broken gate fails the loop bound, not the suite.
        mock.treeGateEnabled = false
        var iterations = 0
        while model.childrenByPath.values.contains(.loading), iterations < 10_000 {
            mock.releaseAllTreeFetches()
            await Task.yield()
            iterations += 1
        }
        for task in tasks { await task.value }

        XCTAssertEqual(mock.treeInFlightPeak, 4, "the cap held for the whole cascade")
        for path in paths {
            guard case .loaded = model.childrenByPath[path] else {
                return XCTFail("\(path) must finish loading after the drain")
            }
        }
    }

    // MARK: - pure helpers

    func testAncestorChainComputesEveryLevelUnderTheRoot() {
        XCTAssertEqual(
            ScansModel.ancestorChain(of: "\(root)/a/b/c.rs", under: root),
            [root, "\(root)/a", "\(root)/a/b"]
        )
        // A direct child needs only the root expanded.
        XCTAssertEqual(ScansModel.ancestorChain(of: "\(root)/a", under: root), [root])
        // The root itself is already visible.
        XCTAssertEqual(ScansModel.ancestorChain(of: root, under: root), [])
    }

    func testAncestorChainRejectsPathsOutsideTheRootComponentAware() {
        XCTAssertNil(ScansModel.ancestorChain(of: "/elsewhere/x", under: root))
        // The classic prefix trap: /a/bc is NOT under /a/b.
        XCTAssertNil(ScansModel.ancestorChain(of: root + "extra/x", under: root))
    }

    func testResidualSizeIsParentMinusChildrenAndNeverNegative() {
        let children = [entry("\(root)/a", disk: 60), entry("\(root)/b", disk: 30)]
        XCTAssertEqual(ScansModel.residualSize(parent: 100, children: children), 10)
        // Exact sum: no residual row.
        XCTAssertEqual(ScansModel.residualSize(parent: 90, children: children), 0)
        // Children over-sum the parent (hardlink dedup quirks): clamp to 0,
        // never wrap to a garbage residual.
        XCTAssertEqual(ScansModel.residualSize(parent: 50, children: children), 0)
        XCTAssertEqual(ScansModel.residualSize(parent: 5, children: []), 5)
    }

    // MARK: - reveal (treemap → tree)

    func testRevealLoadsTheWholeAncestorChainAndPublishes() async throws {
        let mock = MockAPIClient()
        let scan = makeScan()
        let sub = "\(root)/sub"
        let deep = "\(sub)/deep"
        let target = "\(deep)/big.bin"
        mock.treeChildren[root] = [entry(sub, dir: true, disk: 80)]
        mock.treeChildren[sub] = [entry(deep, dir: true, disk: 60)]
        mock.treeChildren[deep] = [entry(target, disk: 50)]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.revealInTree(path: target)
        let reveal = try XCTUnwrap(model.pendingTreeReveal)
        XCTAssertEqual(reveal.path, target)
        XCTAssertEqual(reveal.chain, [root, sub, deep])
        // Every level along the chain is loaded, so the outline can expand
        // without racing a fetch.
        for level in reveal.chain {
            guard case .loaded = model.childrenByPath[level] else {
                return XCTFail("\(level) must be loaded before the reveal publishes")
            }
        }
    }

    func testRevealOutsideTheScanRootIsANoOp() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.treeChildren[root] = []
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)

        await model.revealInTree(path: "/elsewhere/file.bin")
        XCTAssertNil(model.pendingTreeReveal)
    }

    // MARK: - double-click re-root

    func testReRootRebuildsTheFullStackForAnArbitraryJump() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let deep = "\(root)/a/b"
        mock.treeChildren[root] = []
        mock.treemapsByRoot = [
            nil: TreemapLayout(rootPath: root, totalSize: 100, rects: []),
            deep: TreemapLayout(rootPath: deep, totalSize: 40, rects: []),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)

        // Jump two levels in one double-click: the stack carries the
        // intermediate so breadcrumbs read root ▸ a ▸ b.
        await model.reRoot(to: deep)
        XCTAssertEqual(model.treemapRootStack, ["\(root)/a", deep])
        XCTAssertEqual(model.treemap?.rootPath, deep)
        XCTAssertEqual(model.breadcrumbs, [root, "\(root)/a", deep])
    }

    // Regression (2026-09-02 smoke crash): rapid Escape fired two drillOut
    // calls that overlapped across the treemap fetch's await; each passed
    // the non-empty guard, then both removeLast()'d — the second trapped on
    // an empty stack. With the treemap gate forcing the overlap, this test
    // would CRASH pre-fix (removeLast) and passes post-fix (snapshot+assign).
    func testConcurrentDrillOutFromOneLevelDoesNotCrash() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let sub = "\(root)/sub"
        mock.treeChildren[root] = []
        mock.treemapsByRoot = [
            nil: TreemapLayout(rootPath: root, totalSize: 100, rects: []),
            sub: TreemapLayout(rootPath: sub, totalSize: 40, rects: []),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)
        await model.drillIn(to: sub)
        XCTAssertEqual(model.treemapRootStack, [sub], "one level deep")

        // Park every treemap fetch so both drillOuts suspend mid-flight.
        mock.treemapGateEnabled = true
        async let first: Void = model.drillOut()
        async let second: Void = model.drillOut()
        // Wait for both to park (no sleeps).
        while mock.parkedTreemapFetches < 2 { await Task.yield() }
        mock.releaseAllTreemapFetches()
        _ = await (first, second)

        // No trap; the stack settled empty (both went up one level from [sub]).
        XCTAssertEqual(model.treemapRootStack, [], "settled at the scan root, no crash")
    }

    func testReRootToScanRootClearsTheStack() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        let sub = "\(root)/sub"
        mock.treeChildren[root] = []
        mock.treemapsByRoot = [
            nil: TreemapLayout(rootPath: root, totalSize: 100, rects: []),
            sub: TreemapLayout(rootPath: sub, totalSize: 40, rects: []),
        ]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)
        await model.reRoot(to: sub)
        XCTAssertEqual(model.treemapRootStack, [sub])

        await model.reRoot(to: root)
        XCTAssertEqual(model.treemapRootStack, [])
        XCTAssertEqual(model.treemap?.rootPath, root)
    }

    func testReRootFetchFailureLeavesTheStackAlone() async {
        let mock = MockAPIClient()
        let scan = makeScan()
        mock.treeChildren[root] = []
        mock.treemapsByRoot = [nil: TreemapLayout(rootPath: root, totalSize: 100, rects: [])]
        let model = await makeModel(mock, scan: scan)
        await model.select(scanID: scan.id)
        await model.loadTreemap(scanID: scan.id, width: 800, height: 600)

        await model.reRoot(to: "\(root)/unserved") // mock 404s → fetch fails
        XCTAssertEqual(model.treemapRootStack, [], "stack only moves when the fetch lands")
        XCTAssertEqual(model.treemap?.rootPath, root)
        XCTAssertNotNil(model.lastError)
    }

    func testLowerTabDefaultsToFolders() {
        XCTAssertEqual(ScansModel(client: MockAPIClient()).lowerTab, .folders)
    }
}
