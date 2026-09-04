// MockAPIClient — the ONE approved mock, at the network boundary
// (Testing standard: mock at boundaries, never internals).

import Foundation
@testable import PhantomCore

final class MockAPIClient: APIClientProtocol, @unchecked Sendable {
    var failWith: APIError?

    private func checkFailure() throws {
        if let failWith { throw failWith }
    }

    func health() async throws -> Bool {
        try checkFailure()
        return true
    }

    // MARK: - Scans

    var scans: [Scan] = []
    /// Queued getScan responses per id, popped one per call — a scripted
    /// progress timeline for the poller. An EXHAUSTED queue throws, so a
    /// poll loop that fails to stop at the terminal snapshot errors out
    /// instead of hanging the test.
    var scanUpdates: [UUID: [Scan]] = [:]
    var getScanCallCount = 0
    /// Treemap layouts keyed by requested root (nil = scan root).
    var treemapsByRoot: [String?: TreemapLayout] = [:]
    /// Every root= the client asked for, in order (nil = scan root).
    var treemapRootsRequested: [String?] = []
    var lastTreemapWidth: Double?
    var lastTreemapHeight: Double?
    var treeChildren: [String?: [ScanEntry]] = [:]
    var entriesByPath: [String: ScanEntry] = [:]
    var getEntryCallCount = 0
    var typeTotals: [FileTypeTotal] = []
    /// When true, getTypes answers the API's scan-still-running 409.
    var typesConflict = false
    /// File pages keyed by request cursor (nil = first page).
    var filePages: [String?: FilePage] = [:]
    /// Every cursor the client sent, in order (nil = first page).
    var fileCursorsRequested: [String?] = []
    var lastFilesFileType: String?
    var lastFilesSearch: String?

    func startScan(rootPath: String) async throws -> Scan {
        try checkFailure()
        let scan = Scan(
            id: UUID(), rootPath: rootPath, status: .running, startedAt: Date(),
            finishedAt: nil, totalDiskSize: 0, totalLogicalSize: 0,
            fileCount: 0, dirCount: 0, errorCount: 0, unreadablePaths: [],
            progress: Scan.Progress(filesSeen: 0, bytesSeen: 0, currentPath: rootPath)
        )
        scans.insert(scan, at: 0)
        return scan
    }

    func listScans() async throws -> [Scan] {
        try checkFailure()
        return scans
    }

    func getScan(id: UUID) async throws -> Scan {
        try checkFailure()
        getScanCallCount += 1
        if var queue = scanUpdates[id] {
            guard !queue.isEmpty else {
                throw APIError.httpError(status: 500, message: "mock scan update queue exhausted")
            }
            let next = queue.removeFirst()
            scanUpdates[id] = queue
            if let idx = scans.firstIndex(where: { $0.id == id }) { scans[idx] = next }
            return next
        }
        guard let scan = scans.first(where: { $0.id == id }) else {
            throw APIError.httpError(status: 404, message: "not found: scan \(id)")
        }
        return scan
    }

    func cancelScan(id: UUID) async throws -> Scan {
        try checkFailure()
        guard let idx = scans.firstIndex(where: { $0.id == id }) else {
            throw APIError.httpError(status: 404, message: "not found: scan \(id)")
        }
        guard !scans[idx].isTerminal else {
            throw APIError.httpError(
                status: 409,
                message: "scan \(id) is already \(scans[idx].status.rawValue); cannot cancel"
            )
        }
        var cancelled = scans[idx]
        cancelled.status = .cancelled
        cancelled.finishedAt = Date()
        cancelled.progress = nil
        scans[idx] = cancelled
        return cancelled
    }

    /// When `treemapGateEnabled`, every getTreemap PARKS until
    /// releaseAllTreemapFetches() — lets a test force two navigations
    /// (e.g. rapid Escape → drillOut) to overlap deterministically and
    /// catch the removeLast-after-await race, no sleeps.
    var treemapGateEnabled = false
    private var treemapContinuations: [CheckedContinuation<Void, Never>] = []
    var parkedTreemapFetches: Int { withTreeLock { treemapContinuations.count } }
    func releaseAllTreemapFetches() {
        let parked = withTreeLock {
            let p = treemapContinuations
            treemapContinuations = []
            return p
        }
        parked.forEach { $0.resume() }
    }

    func getTreemap(
        scanID: UUID, root: String?, width: Double?, height: Double?, maxDepth: Int?
    ) async throws -> TreemapLayout {
        try checkFailure()
        treemapRootsRequested.append(root)
        lastTreemapWidth = width
        lastTreemapHeight = height
        if treemapGateEnabled {
            await withCheckedContinuation { cont in
                withTreeLock { treemapContinuations.append(cont) }
            }
        }
        guard let layout = treemapsByRoot[root] else {
            throw APIError.httpError(status: 404, message: "not found: path \(root ?? "<root>") in scan \(scanID)")
        }
        return layout
    }

    private(set) var getTreeCallCount = 0
    /// Concurrency probe: when `treeGateEnabled`, every getTree PARKS until
    /// releaseAllTreeFetches(), and the peak number simultaneously parked
    /// is recorded — how the fetch-concurrency cap is asserted without
    /// sleeps. Lock-protected: calls arrive off the main actor.
    var treeGateEnabled = false
    private let treeLock = NSLock()
    private var treeContinuations: [CheckedContinuation<Void, Never>] = []
    private(set) var treeInFlight = 0
    private(set) var treeInFlightPeak = 0

    /// Synchronous locking helper — NSLock's lock()/unlock() are barred in
    /// async contexts under Swift 6; a sync closure hop is the sanctioned
    /// shape (nothing suspends while the lock is held).
    private func withTreeLock<T>(_ body: () -> T) -> T {
        treeLock.lock()
        defer { treeLock.unlock() }
        return body()
    }

    var parkedTreeFetches: Int {
        withTreeLock { treeContinuations.count }
    }

    func releaseAllTreeFetches() {
        let parked = withTreeLock {
            let parked = treeContinuations
            treeContinuations = []
            return parked
        }
        parked.forEach { $0.resume() }
    }

    func getTree(scanID: UUID, path: String?) async throws -> [ScanEntry] {
        try checkFailure()
        withTreeLock { getTreeCallCount += 1 }
        if treeGateEnabled {
            withTreeLock {
                treeInFlight += 1
                treeInFlightPeak = max(treeInFlightPeak, treeInFlight)
            }
            await withCheckedContinuation { cont in
                withTreeLock { treeContinuations.append(cont) }
            }
            withTreeLock { treeInFlight -= 1 }
        }
        guard let children = treeChildren[path] else {
            throw APIError.httpError(status: 404, message: "not found: path \(path ?? "<root>") in scan \(scanID)")
        }
        return children
    }

    func getEntry(scanID: UUID, path: String) async throws -> ScanEntry {
        try checkFailure()
        getEntryCallCount += 1
        guard let entry = entriesByPath[path] else {
            throw APIError.httpError(status: 404, message: "not found: path \(path) in scan \(scanID)")
        }
        return entry
    }

    func listFiles(
        scanID: UUID, fileType: String?, search: String?, sort: String?,
        limit: Int?, cursor: String?
    ) async throws -> FilePage {
        try checkFailure()
        fileCursorsRequested.append(cursor)
        lastFilesFileType = fileType
        lastFilesSearch = search
        // Unscripted FIRST page mirrors the API's empty-scan answer; an
        // unscripted CONTINUATION cursor stays loud — a poll or pagination
        // loop that invents cursors must fail the test, not spin.
        if cursor == nil, filePages.isEmpty {
            return FilePage(files: [], nextCursor: nil)
        }
        guard let page = filePages[cursor] else {
            throw APIError.httpError(status: 400, message: "cursor is not a valid continuation token")
        }
        return page
    }

    /// The hotspots summary to serve; nil means "nothing classified" and
    /// serves the honest all-empty summary (the API's terminal default).
    var hotspotsSummary: HotspotsSummary?
    /// When true, getHotspots answers the API's scan-still-running 409.
    var hotspotsConflict = false

    func getHotspots(scanID: UUID) async throws -> HotspotsSummary {
        try checkFailure()
        if hotspotsConflict {
            throw APIError.httpError(
                status: 409,
                message: "scan \(scanID) is still running; results are available once it finishes"
            )
        }
        return hotspotsSummary ?? HotspotsSummary(
            groups: [], reclaimEstimate: 0, reviewDiskSize: 0,
            cloudDataloadedLogicalSize: 0, cloudDataloadedDiskSize: 0
        )
    }

    func getTypes(scanID: UUID) async throws -> [FileTypeTotal] {
        try checkFailure()
        if typesConflict {
            throw APIError.httpError(
                status: 409,
                message: "scan \(scanID) is still running; results are available once it finishes"
            )
        }
        return typeTotals
    }
}
