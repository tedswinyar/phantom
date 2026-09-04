// The scans view model. Lives in PhantomCore (not the app target) so it is
// unit-testable at the network boundary: tests drive it with MockAPIClient
// and assert the lifecycle (start → poll → terminal), cancel, and treemap
// drill-down paths without a UI or a real server. Follows NotesModel's
// pattern: @Observable, client injected through the one boundary protocol.

import Foundation
import Observation

/// View model for the scan list and the treemap drill-down state. Talks to
/// the API through the APIClientProtocol boundary only.
@MainActor
@Observable
public final class ScansModel {
    public var scans: [Scan] = []
    public var connectionState: ConnectionState = .connecting
    public var lastError: String?

    /// The scan whose results the detail surfaces (treemap, files, types,
    /// inspector) are showing. All of that state is per-scan and resets on
    /// selection change.
    public private(set) var selectedScanID: UUID?
    public var selectedScan: Scan? {
        selectedScanID.flatMap { id in scans.first { $0.id == id } }
    }

    /// The current page window of GET /scans/{id}/files, filtered by
    /// `fileTypeFilter`/`fileSearch`. Grows page-by-page via loadMoreFiles().
    public private(set) var files: [ScanEntry] = []
    /// Continuation token for the next files page; nil == listing complete.
    public private(set) var filesNextCursor: String?
    /// True while a files page is in flight (guards double-fetch).
    public private(set) var filesLoading = false
    /// Views bind these and call loadFiles() to apply; empty == no filter.
    public var fileTypeFilter: String?
    public var fileSearch: String = ""

    /// The legend chip currently highlighting the treemap (non-matching
    /// tiles dim) and filtering Largest Files. `.type(nil)` is the
    /// no-extension bucket — a real slot when it ranks top-nine.
    public enum LegendSelection: Equatable, Sendable {
        case none
        case type(String?)
    }
    public private(set) var legendSelection: LegendSelection = .none

    /// Inspector selection. The path is set immediately (treemap/list
    /// highlight); the entry follows from cache or a fetch.
    public private(set) var selectedEntryPath: String?
    public private(set) var selectedEntry: ScanEntry?

    /// Per-type totals for the selected scan; empty until loaded.
    public private(set) var typeTotals: [FileTypeTotal] = []
    /// True when the last types fetch answered 409 — the scan is still
    /// running and totals will exist once it finishes. Not an error.
    public private(set) var typesStillRunning = false

    /// The reclaimable-space summary for the selected scan; nil until
    /// loaded. A loaded-but-empty summary means the classifier had nothing
    /// to say — render that honestly, not as a failure.
    public private(set) var hotspots: HotspotsSummary?
    /// True when the last hotspots fetch answered 409 — same expected
    /// still-running state as typesStillRunning, not an error.
    public private(set) var hotspotsStillRunning = false

    /// The Full Disk Access grant, as last probed. Drives the quiet
    /// indicator and the scan-error callout; `.undetermined` shows nothing.
    public private(set) var fdaStatus: FullDiskAccess.Status = .undetermined

    /// Inspector visibility — lives here (not view @State) so the View
    /// menu command can drive it. A viewing preference, not per-scan data.
    public var showInspector = true
    /// The New Haunt sheet, model-owned so the toolbar button, the File
    /// menu's Cmd-N, and the sidebar's add button all drive one state.
    public var showScanSheet = false

    /// Which lower-pane tab is showing. Sticky across scan selection —
    /// it is a viewing preference, not per-scan data.
    public var lowerTab: LowerTab = .folders

    public enum LowerTab: String, CaseIterable, Sendable {
        case folders = "Folders"
        case largestFiles = "Largest Files"
        case reclaimable = "Reclaimable"
    }

    /// The Folders tree's lazy children cache, keyed by directory path.
    /// One GET /tree per FIRST disclosure; entries are re-sorted diskSize
    /// descending client-side (the server returns ORDER BY path). Reset
    /// with the rest of the per-scan state.
    public enum ChildrenState: Equatable {
        case loading
        case loaded([ScanEntry])
        case failed(String)
    }
    public private(set) var childrenByPath: [String: ChildrenState] = [:]

    /// A treemap→tree reveal request the outline view consumes: expand the
    /// chain (outermost first), scroll to and select the path. A fresh id
    /// per request so the view can distinguish repeat reveals of one path.
    public struct TreeReveal: Equatable {
        public let id: UUID
        public let path: String
        /// Ancestor directories to expand, outermost (scan root) first.
        public let chain: [String]
    }
    public private(set) var pendingTreeReveal: TreeReveal?

    /// The current treemap layout (nil until a scan's treemap is loaded).
    public private(set) var treemap: TreemapLayout?
    /// Drill-down path: each element is a directory the user drilled into,
    /// deepest last. Empty means the treemap shows the scan root.
    public private(set) var treemapRootStack: [String] = []
    /// The directory the treemap is currently rooted at (nil = scan root).
    public var treemapRoot: String? { treemapRootStack.last }

    public enum ConnectionState: Equatable {
        case connecting
        case connected
        case failed(String)
    }

    private var client: APIClientProtocol?
    /// The FDA canary probe — the filesystem boundary, injected for tests.
    private let fdaProbe: FileAccessProbing
    /// Delay between progress polls. Injected so tests run at zero delay.
    private let pollInterval: Duration
    private var pollTasks: [UUID: Task<Void, Never>] = [:]

    /// Tree-fetch concurrency gate: an Option-click expand-all can request
    /// children for every visible directory at once; at most this many
    /// GET /tree calls run concurrently, the rest queue FIFO.
    private static let maxTreeFetches = 4
    private var activeTreeFetches = 0
    private var treeFetchWaiters: [CheckedContinuation<Void, Never>] = []
    /// In-flight children fetches, so concurrent requests for one path
    /// coalesce into one GET (and revealInTree can await a disclosure's
    /// fetch instead of racing it).
    private var childrenTasks: [String: Task<Void, Never>] = [:]

    /// Treemap fetch parameters, remembered so drill in/out refetches at the
    /// same view size the app last asked for.
    private var treemapScanID: UUID?
    private var treemapWidth: Double?
    private var treemapHeight: Double?
    private var treemapMaxDepth: Int?

    public init(
        pollInterval: Duration = .seconds(1),
        fdaProbe: FileAccessProbing = FileSystemAccessProbe()
    ) {
        self.pollInterval = pollInterval
        self.fdaProbe = fdaProbe
    }

    /// Test seam: inject a client and skip process supervision. `@testable`
    /// callers construct the model already "connected" to a mock. The
    /// default probe is only consulted when refreshFDAStatus() runs, which
    /// tests trigger explicitly with their own mock probe.
    init(
        client: APIClientProtocol,
        connectionState: ConnectionState = .connected,
        pollInterval: Duration = .zero,
        fdaProbe: FileAccessProbing = FileSystemAccessProbe()
    ) {
        self.client = client
        self.connectionState = connectionState
        self.pollInterval = pollInterval
        self.fdaProbe = fdaProbe
    }

    /// Re-run the canary probe (on connect, and whenever the user returns
    /// from System Settings — the app re-checks on activation).
    public func refreshFDAStatus() {
        fdaStatus = FullDiskAccess.status(probe: fdaProbe)
    }

    /// The scan-detail callout decision, all four cells pinned by tests:
    /// only a scan that RECORDED unreadable items while the grant is
    /// provably missing earns the FDA hint. Errors with the grant present
    /// (or with no evidence about it) show the honest error count alone —
    /// FDA does not fix everything and the copy must not claim it does.
    public func shouldShowFDAHint(for scan: Scan) -> Bool {
        scan.errorCount > 0 && fdaStatus == .notGranted
    }

    /// Ensure the bundled server is running, then load.
    public func connect() async {
        // Environment override (PHANTOM_API_URL) wins — dev workflows and
        // e2e tests point the app at a server they manage.
        refreshFDAStatus()
        if ProcessInfo.processInfo.environment["PHANTOM_API_URL"] != nil {
            self.client = APIClient.fromEnvironment()
            await refresh()
            return
        }

        do {
            let url = try await APIServerManager.shared.startIfNeeded()
            self.client = APIClient(
                baseURL: url,
                apiKey: ProcessInfo.processInfo.environment["PHANTOM_API_KEY"]
                    ?? APIClient.readKeyFile(path: ProcessInfo.processInfo.environment["PHANTOM_KEY_FILE"])
                    ?? ""
            )
        } catch {
            connectionState = .failed(
                "Could not start phantom-api. See \(APIServerManager.logFileURL.path)"
            )
            return
        }
        await refresh()
    }

    public func refresh() async {
        guard let client else { return }
        do {
            scans = try await client.listScans()
            connectionState = .connected
            lastError = nil
        } catch {
            connectionState = .failed(error.localizedDescription)
            return
        }
        // Nothing selected but results exist: open on the newest terminal
        // scan (the list is newest-first) instead of an empty detail pane.
        if selectedScanID == nil,
           let newest = scans.first(where: { $0.isTerminal }) {
            await select(scanID: newest.id)
        }
    }

    // MARK: - Lifecycle

    /// Start a scan: the 202 response's running scan appears at the top of
    /// the list immediately and progress polling begins.
    public func startScan(rootPath: String) async {
        guard let client else { return }
        do {
            let scan = try await client.startScan(rootPath: rootPath)
            scans.insert(scan, at: 0)
            lastError = nil
            startPolling(id: scan.id)
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Begin (or resume, e.g. after relaunch) polling a running scan. No-op
    /// if a poll for this scan is already in flight.
    public func startPolling(id: UUID) {
        guard pollTasks[id] == nil else { return }
        pollTasks[id] = Task { [weak self] in
            await self?.pollUntilTerminal(id: id)
            self?.pollTasks[id] = nil
        }
    }

    public func stopPolling(id: UUID) {
        pollTasks[id]?.cancel()
        pollTasks[id] = nil
    }

    /// Test seam: await the in-flight poll for a scan, so tests observe the
    /// terminal state deterministically instead of sleeping.
    func awaitPolling(id: UUID) async {
        await pollTasks[id]?.value
    }

    /// Poll GET /scans/{id} until the scan turns terminal (progress goes
    /// null / status leaves `running`). Each snapshot updates the list row,
    /// so progress counters are live in the UI.
    func pollUntilTerminal(id: UUID) async {
        guard let client else { return }
        while !Task.isCancelled {
            do {
                let updated = try await client.getScan(id: id)
                upsert(updated)
                if updated.isTerminal { return }
            } catch {
                lastError = error.localizedDescription
                return
            }
            try? await Task.sleep(for: pollInterval)
        }
    }

    /// Request cancellation. The API answers 202 with the (possibly still
    /// running) scan view — the walker stops cooperatively, so polling
    /// continues until the terminal `cancelled` lands. A 409 (already
    /// terminal) surfaces as an error without touching the row.
    public func cancel(id: UUID) async {
        guard let client else { return }
        do {
            let view = try await client.cancelScan(id: id)
            upsert(view)
            lastError = nil
            if !view.isTerminal { startPolling(id: id) }
        } catch {
            lastError = error.localizedDescription
        }
    }

    // MARK: - Selection

    /// Select a scan for the detail surfaces. Per-scan state resets; a
    /// terminal scan loads its types and first files page, a running one
    /// (re)starts progress polling instead.
    public func select(scanID: UUID?) async {
        guard selectedScanID != scanID else { return }
        selectedScanID = scanID
        files = []
        filesNextCursor = nil
        fileTypeFilter = nil
        fileSearch = ""
        typeTotals = []
        typesStillRunning = false
        legendSelection = .none
        hotspots = nil
        hotspotsStillRunning = false
        childrenByPath = [:]
        childrenTasks = [:]
        pendingTreeReveal = nil
        treemap = nil
        treemapRootStack = []
        treemapScanID = nil
        selectedEntryPath = nil
        selectedEntry = nil

        guard let scan = selectedScan else { return }
        if scan.isTerminal {
            // Cancelled/failed scans persist no results; these come back
            // honestly empty rather than erroring.
            await loadTypes(scanID: scan.id)
            await loadHotspots(scanID: scan.id)
            await loadFiles()
            await loadChildren(path: scan.rootPath)
        } else {
            startPolling(id: scan.id)
        }
    }

    /// Select an entry for the inspector. The current files page is the
    /// cache; anything else (directories, rows outside the page) fetches
    /// through GET /scans/{id}/entry.
    public func selectEntry(path: String?) async {
        selectedEntryPath = path
        guard let path else {
            selectedEntry = nil
            return
        }
        if let cached = files.first(where: { $0.path == path }) {
            selectedEntry = cached
            return
        }
        guard let client, let scanID = selectedScanID else { return }
        do {
            selectedEntry = try await client.getEntry(scanID: scanID, path: path)
            lastError = nil
        } catch {
            selectedEntry = nil
            lastError = error.localizedDescription
        }
    }

    // MARK: - Files (paged)

    /// Load the FIRST page for the current filter/search, replacing the list.
    public func loadFiles() async {
        guard let client, let scanID = selectedScanID, !filesLoading else { return }
        filesLoading = true
        defer { filesLoading = false }
        do {
            let page = try await client.listFiles(
                scanID: scanID,
                fileType: normalized(fileTypeFilter),
                search: normalized(fileSearch),
                sort: nil, limit: nil, cursor: nil
            )
            files = page.files
            filesNextCursor = page.nextCursor
            lastError = nil
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Append the next page. A nil cursor means the listing is COMPLETE —
    /// calling again is a no-op, never a refetch of page one.
    public func loadMoreFiles() async {
        guard let client, let scanID = selectedScanID, !filesLoading else { return }
        guard let cursor = filesNextCursor else { return }
        filesLoading = true
        defer { filesLoading = false }
        do {
            let page = try await client.listFiles(
                scanID: scanID,
                fileType: normalized(fileTypeFilter),
                search: normalized(fileSearch),
                sort: nil, limit: nil, cursor: cursor
            )
            files.append(contentsOf: page.files)
            filesNextCursor = page.nextCursor
            lastError = nil
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Scroll-triggered paging: the file table reports each row as it
    /// scrolls into view; the LAST loaded row appearing while a
    /// continuation cursor remains means the user reached the bottom —
    /// fetch the next page. Any other row is a no-op.
    public func fileRowAppeared(path: String) async {
        guard path == files.last?.path, filesNextCursor != nil else { return }
        await loadMoreFiles()
    }

    /// The denominator for the file-list summary: the scan's TOTAL file
    /// count — but only when the listing is unfiltered; a type filter or
    /// search changes the population and the server doesn't report a
    /// filtered total.
    public var filesTotalCount: UInt64? {
        guard normalized(fileTypeFilter) == nil, normalized(fileSearch) == nil else { return nil }
        return selectedScan?.fileCount
    }

    private func normalized(_ value: String?) -> String? {
        guard let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines),
              !trimmed.isEmpty else { return nil }
        return trimmed
    }

    // MARK: - Legend (the file-type color axis)

    /// The top-nine file types of the selected scan, in legend-slot order.
    /// Derived from typeTotals (server-sorted by disk desc), so it resets
    /// with the rest of the per-scan state for free. Slots can include nil
    /// — the no-extension bucket earns a color like any other type.
    public var legendSlots: [String?] {
        Array(typeTotals.prefix(9).map(\.fileType))
    }

    /// The legend slot (0..<9) for a file type, or nil for "other". One
    /// mapping feeds the treemap tiles, the tree swatches, the file-list
    /// icons, and the legend chips — a type can never wear two colors.
    /// Stable within a scan because typeTotals only changes on selection.
    public func legendSlot(for fileType: String?) -> Int? {
        legendSlots.firstIndex(of: fileType)
    }

    /// Legend chip click: first click highlights the type on the map AND
    /// filters Largest Files to it; the same chip again clears both. The
    /// no-extension bucket can only highlight — the files endpoint has no
    /// way to say "extensionless", so the filter is left alone for it.
    public func toggleLegendChip(_ fileType: String?) async {
        if legendSelection == .type(fileType) {
            legendSelection = .none
            if fileTypeFilter != nil {
                fileTypeFilter = nil
                await loadFiles()
            }
            return
        }
        legendSelection = .type(fileType)
        if let fileType {
            fileTypeFilter = fileType
            await loadFiles()
        }
    }

    // MARK: - Folders tree

    /// Load a directory's children on FIRST disclosure. A second caller for
    /// an in-flight path AWAITS the same fetch (revealInTree depends on
    /// this); loaded and failed paths are no-ops (use retryChildren to
    /// force). Results are re-sorted diskSize descending — the tree is a
    /// size view; the server returns ORDER BY path.
    public func loadChildren(path: String) async {
        if let inFlight = childrenTasks[path] {
            await inFlight.value
            return
        }
        guard childrenByPath[path] == nil else { return }
        childrenByPath[path] = .loading
        let task = Task { [weak self] in _ = await self?.fetchChildren(path: path) }
        childrenTasks[path] = task
        await task.value
        childrenTasks[path] = nil
    }

    private func fetchChildren(path: String) async {
        guard let client, let scanID = selectedScanID else { return }
        await acquireTreeFetchSlot()
        defer { releaseTreeFetchSlot() }
        do {
            let children = try await client.getTree(scanID: scanID, path: path)
            // The selection may have moved while this was queued behind the
            // gate; never write one scan's children into another's cache.
            guard selectedScanID == scanID else { return }
            childrenByPath[path] = .loaded(children.sorted { $0.diskSize > $1.diskSize })
        } catch {
            guard selectedScanID == scanID else { return }
            childrenByPath[path] = .failed(error.localizedDescription)
        }
    }

    /// The inline retry row's action: clear the failed state and refetch.
    public func retryChildren(path: String) async {
        guard case .failed = childrenByPath[path] else { return }
        childrenByPath[path] = nil
        await loadChildren(path: path)
    }

    /// FIFO concurrency gate over GET /tree (see maxTreeFetches).
    private func acquireTreeFetchSlot() async {
        if activeTreeFetches < Self.maxTreeFetches {
            activeTreeFetches += 1
            return
        }
        await withCheckedContinuation { treeFetchWaiters.append($0) }
    }

    private func releaseTreeFetchSlot() {
        if treeFetchWaiters.isEmpty {
            activeTreeFetches -= 1
        } else {
            // Hand the slot straight to the next waiter; the count stays.
            treeFetchWaiters.removeFirst().resume()
        }
    }

    /// The directories to expand, outermost first, to make `path` visible
    /// in a tree rooted at `root`: the root itself and every strict
    /// ancestor of `path` below it. nil when `path` is not under `root`
    /// (component-boundary aware: "/a/bc" is NOT under "/a/b"). The root
    /// itself yields [] — it is already visible.
    public static func ancestorChain(of path: String, under root: String) -> [String]? {
        if path == root { return [] }
        guard path.hasPrefix(root + "/") else { return nil }
        let relative = path.dropFirst(root.count + 1)
        var chain = [root]
        var current = root
        let components = relative.split(separator: "/")
        // Every component except the last is an ancestor directory.
        for component in components.dropLast() {
            current += "/\(component)"
            chain.append(current)
        }
        return chain
    }

    /// The "(smaller files)" residual: what a directory's aggregate holds
    /// beyond its persisted children — the wire's <1 MiB folding policy made
    /// honest. Never negative: hardlink dedup quirks can make children sum
    /// past the parent, and that renders as no residual, not garbage.
    public static func residualSize(parent: UInt64, children: [ScanEntry]) -> UInt64 {
        let sum = children.reduce(UInt64(0)) { $0 + $1.diskSize }
        return parent > sum ? parent - sum : 0
    }

    /// Treemap→tree sync: ensure every level of `path`'s ancestor chain is
    /// loaded (fetching uncached ones), then publish the reveal for the
    /// outline view to expand, scroll, and select. No-op for paths outside
    /// the selected scan's root.
    public func revealInTree(path: String) async {
        guard let scan = selectedScan,
              let chain = Self.ancestorChain(of: path, under: scan.rootPath)
        else { return }
        for ancestor in chain {
            await loadChildren(path: ancestor)
        }
        pendingTreeReveal = TreeReveal(id: UUID(), path: path, chain: chain)
    }

    /// Tree double-click: re-root the treemap at an ARBITRARY directory,
    /// rebuilding the full drill stack so the breadcrumb trail reads
    /// root ▸ … ▸ target (drillIn only ever appends one level). The stack
    /// only moves when the fetch lands.
    public func reRoot(to path: String) async {
        guard let scan = selectedScan else { return }
        if path == scan.rootPath {
            guard !treemapRootStack.isEmpty else { return }
            guard await fetchTreemap(root: nil) else { return }
            treemapRootStack = []
            return
        }
        guard let chain = Self.ancestorChain(of: path, under: scan.rootPath) else { return }
        guard await fetchTreemap(root: path) else { return }
        // chain = [root, intermediates…]; the stack excludes the root and
        // includes the target.
        treemapRootStack = Array(chain.dropFirst()) + [path]
    }

    // MARK: - Results

    /// Load per-type totals. A 409 means the scan is still running — an
    /// expected state (typesStillRunning), not an error.
    public func loadTypes(scanID: UUID) async {
        guard let client else { return }
        do {
            typeTotals = try await client.getTypes(scanID: scanID)
            typesStillRunning = false
            lastError = nil
        } catch APIError.httpError(status: 409, message: _) {
            typeTotals = []
            typesStillRunning = true
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Load the reclaimable-space summary. A 409 means the scan is still
    /// running — an expected state (hotspotsStillRunning), not an error.
    public func loadHotspots(scanID: UUID) async {
        guard let client else { return }
        do {
            hotspots = try await client.getHotspots(scanID: scanID)
            hotspotsStillRunning = false
            lastError = nil
        } catch APIError.httpError(status: 409, message: _) {
            hotspots = nil
            hotspotsStillRunning = true
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Load the treemap rooted at the scan root, resetting any drill-down.
    /// The app passes its actual view size; the server lays out to fit.
    public func loadTreemap(
        scanID: UUID, width: Double? = nil, height: Double? = nil, maxDepth: Int? = nil
    ) async {
        treemapScanID = scanID
        treemapWidth = width
        treemapHeight = height
        treemapMaxDepth = maxDepth
        treemapRootStack = []
        await fetchTreemap(root: nil)
    }

    /// Drill into a directory: refetch the layout re-rooted (and re-laid-out
    /// server-side) at `path`, pushing it onto the drill stack on success.
    public func drillIn(to path: String) async {
        guard await fetchTreemap(root: path) else { return }
        // Idempotent under re-entrancy: two drills to the same path (the
        // await lets a second call interleave) must not stack duplicates.
        if treemapRootStack.last != path {
            treemapRootStack.append(path)
        }
    }

    /// Drill back out one level (to the parent root, or the scan root).
    ///
    /// Snapshot-then-assign, NOT removeLast-after-await: `fetchTreemap`
    /// suspends, and a second drillOut (rapid Escape) or a reRoot can run
    /// during the suspension. `removeLast()` re-evaluated against a stack
    /// that another call already emptied traps ("remove last from an empty
    /// collection" — a real crash, ScansModel.swift drillOut, 2026-09-02
    /// smoke). Computing the target up front and assigning it — the same
    /// shape as `navigate(toCrumb:)` — can never trap.
    public func drillOut() async {
        guard !treemapRootStack.isEmpty else { return }
        let target = Array(treemapRootStack.dropLast())
        guard await fetchTreemap(root: target.last) else { return }
        treemapRootStack = target
    }

    /// The drill trail: the scan root, then each drilled directory. Empty
    /// when no scan is selected.
    public var breadcrumbs: [String] {
        guard let scan = selectedScan else { return [] }
        return [scan.rootPath] + treemapRootStack
    }

    /// Jump to a breadcrumb: index 0 is the scan root, index i is the i-th
    /// drilled directory. The stack only moves when the refetch lands; the
    /// last crumb (current root) is a no-op.
    public func navigate(toCrumb index: Int) async {
        let crumbs = breadcrumbs
        guard index >= 0, index < crumbs.count, index != crumbs.count - 1 else { return }
        let newStack = Array(treemapRootStack.prefix(index))
        guard await fetchTreemap(root: newStack.last) else { return }
        treemapRootStack = newStack
    }

    /// Record the treemap view's ACTUAL size for subsequent fetches (drill
    /// in/out, crumb jumps). No fetch happens here: a resize re-scales the
    /// already-fetched layout client-side; only the next navigation asks the
    /// server to lay out at the new size.
    public func updateTreemapViewport(width: Double, height: Double) {
        treemapWidth = width
        treemapHeight = height
    }

    /// Fetch the layout at `root` (nil = scan root) using the remembered
    /// view parameters. Returns false (and surfaces the error) on failure —
    /// the drill stack only moves when the fetch lands.
    @discardableResult
    private func fetchTreemap(root: String?) async -> Bool {
        guard let client, let scanID = treemapScanID else { return false }
        do {
            treemap = try await client.getTreemap(
                scanID: scanID, root: root,
                width: treemapWidth, height: treemapHeight, maxDepth: treemapMaxDepth
            )
            lastError = nil
            return true
        } catch {
            lastError = error.localizedDescription
            return false
        }
    }

    private func upsert(_ scan: Scan) {
        if let idx = scans.firstIndex(where: { $0.id == scan.id }) {
            scans[idx] = scan
        } else {
            scans.insert(scan, at: 0)
        }
    }
}
