// The Folders tree (folders-tree-spec.md): an NSOutlineView in an
// NSViewRepresentable, because lazy children is the native protocol and
// programmatic reveal (expandItem + scrollRowToVisible) IS the treemap→tree
// sync feature — SwiftUI's Table/OutlineGroup has neither. All sync and
// cache logic lives in ScansModel where MockAPIClient tests reach it; this
// file only adapts that state to AppKit and forwards user intent back.
//
// Columns: Name (disclosure + type swatch) · % of Parent (bar + number) ·
// Disk Size (headline, default sort desc) · Logical Size (hideable via the
// header's context menu) · Modified. NO Items column — per-dir counts are
// not on the wire yet; when they land, add a column here and nothing else.

import AppKit
import SwiftUI
import DesignKit
import PhantomCore

// MARK: - SwiftUI wrapper

struct FoldersView: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        if model.selectedScan != nil {
            FolderOutlineView(model: model)
        } else {
            Text(Vocabulary.noSelection)
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: - Tree nodes

/// Reference-typed tree items: NSOutlineView tracks expansion and reload
/// identity by POINTER, so every path keeps one node instance for the life
/// of the scan selection.
final class FolderTreeNode {
    enum Kind {
        case entry(ScanEntry)
        /// The "(smaller files)" leaf: the parent aggregate's remainder
        /// beyond its persisted children — the wire's <1 MiB folding policy
        /// made honest. Not selectable, no swatch.
        case residual(UInt64)
        case loading
        case failure(String)
    }

    var kind: Kind
    /// The entry's path, or a synthetic per-parent identity for the rest.
    let path: String
    /// The directory this node sits under (retry target for failures).
    let parentPath: String
    /// The parent's aggregated diskSize — the % of Parent denominator.
    var parentDiskSize: UInt64

    init(kind: Kind, path: String, parentPath: String, parentDiskSize: UInt64) {
        self.kind = kind
        self.path = path
        self.parentPath = parentPath
        self.parentDiskSize = parentDiskSize
    }

    var entry: ScanEntry? {
        if case .entry(let entry) = kind { return entry }
        return nil
    }

    var diskSize: UInt64 {
        switch kind {
        case .entry(let entry): return entry.displaySize
        case .residual(let size): return size
        case .loading, .failure: return 0
        }
    }
}

// MARK: - Representable

struct FolderOutlineView: NSViewRepresentable {
    let model: ScansModel

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let outline = QuickLookOutlineView()
        outline.onSpace = { [weak coordinator = context.coordinator] in
            guard let path = coordinator?.selectedEntryPath() else { return false }
            QuickLookController.shared.toggle(path: path)
            return true
        }
        outline.dataSource = context.coordinator
        outline.delegate = context.coordinator
        outline.target = context.coordinator
        // A single MOUSE click on a folder re-roots the treemap to it (the
        // map follows the tree, symmetric with clicking a map tile). `action`
        // fires only on clicks, never on keyboard selection changes, so
        // arrow-keying through rows still only selects — no re-root-per-row
        // server thrash (the reason single-click historically did nothing).
        outline.action = #selector(Coordinator.onSingleClick(_:))
        outline.doubleAction = #selector(Coordinator.onDoubleClick(_:))
        outline.usesAlternatingRowBackgroundColors = false
        outline.style = .inset
        outline.rowSizeStyle = .default
        outline.autosaveTableColumns = false
        outline.allowsMultipleSelection = false
        outline.autoresizesOutlineColumn = false

        func column(
            _ id: String, _ title: String, width: CGFloat, sortKey: String? = nil
        ) -> NSTableColumn {
            let col = NSTableColumn(identifier: .init(id))
            col.title = title
            col.width = width
            if let sortKey {
                col.sortDescriptorPrototype = NSSortDescriptor(key: sortKey, ascending: true)
            }
            return col
        }
        let name = column("name", "Name", width: 320, sortKey: "name")
        name.minWidth = 160
        outline.addTableColumn(name)
        outline.outlineTableColumn = name
        outline.addTableColumn(column(
            "percent", "% of Parent",
            width: TreeStyle.barWidth + TreeStyle.percentLabelWidth + Spacing.sm))
        outline.addTableColumn(column("disk", "Disk Size", width: 90, sortKey: "disk"))
        outline.addTableColumn(column("logical", "Logical Size", width: 90, sortKey: "logical"))
        outline.addTableColumn(column("items", "Items", width: 130))
        outline.addTableColumn(column("modified", "Modified", width: 140, sortKey: "modified"))
        // Default sort: Disk Size descending — this is a size view.
        outline.sortDescriptors = [NSSortDescriptor(key: "disk", ascending: false)]

        // Header context menu: hide/show the secondary size column.
        let headerMenu = NSMenu()
        let logicalItem = NSMenuItem(
            title: "Logical Size",
            action: #selector(Coordinator.toggleLogicalColumn(_:)),
            keyEquivalent: ""
        )
        logicalItem.target = context.coordinator
        logicalItem.state = .on
        headerMenu.addItem(logicalItem)
        outline.headerView?.menu = headerMenu

        // Row context menu, populated per clicked row (entry rows only).
        let rowMenu = NSMenu()
        rowMenu.delegate = context.coordinator
        outline.menu = rowMenu

        context.coordinator.outline = outline

        let scroll = NSScrollView()
        scroll.documentView = outline
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        return scroll
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        let coordinator = context.coordinator

        // Read the observed state HERE so SwiftUI re-invokes this update
        // when any of it changes.
        let scanID = model.selectedScanID
        let children = model.childrenByPath
        let selectedPath = model.selectedEntryPath
        let reveal = model.pendingTreeReveal

        if scanID != coordinator.lastScanID {
            coordinator.lastScanID = scanID
            coordinator.resetNodes()
            coordinator.lastChildren = children
            coordinator.outline?.reloadData()
        } else if children != coordinator.lastChildren {
            coordinator.lastChildren = children
            coordinator.reloadPreservingExpansion()
        }

        coordinator.syncSelection(to: selectedPath)

        if let reveal, reveal.id != coordinator.lastRevealID {
            coordinator.lastRevealID = reveal.id
            coordinator.perform(reveal: reveal)
        }
    }

    // MARK: - Coordinator (data source + delegate)

    @MainActor
    final class Coordinator: NSObject, NSOutlineViewDataSource, NSOutlineViewDelegate, NSMenuDelegate {
        let model: ScansModel
        weak var outline: NSOutlineView?

        var lastScanID: UUID?
        var lastChildren: [String: ScansModel.ChildrenState] = [:]
        var lastRevealID: UUID?

        /// Stable node identity per path; expansion state survives reloads
        /// because NSOutlineView keys on these exact instances.
        private var nodesByPath: [String: FolderTreeNode] = [:]
        /// One synthetic node per parent per role, same stability reason.
        private var syntheticNodes: [String: FolderTreeNode] = [:]
        private var isSyncingSelection = false
        /// Active column sort; disk-descending is the canonical default.
        private var sortKey = "disk"
        private var sortAscending = false
        /// Pending settle-debounced re-root (keyboard navigation). Cancelled
        /// and rescheduled on every selection change, so the map re-roots
        /// only once the user STOPS arrowing — not once per row.
        private var reRootDebounce: Task<Void, Never>?
        /// How long the selection must hold still before a keyboard-driven
        /// re-root fires.
        private let reRootSettleDelay = Duration.milliseconds(350)

        init(model: ScansModel) {
            self.model = model
        }

        func resetNodes() {
            nodesByPath = [:]
            syntheticNodes = [:]
        }

        func reloadPreservingExpansion() {
            // Item identity is pointer identity on the cached nodes, so a
            // plain reload re-queries children while keeping expansion.
            outline?.reloadData()
        }

        // MARK: node construction

        private func node(for entry: ScanEntry, parentPath: String, parentSize: UInt64) -> FolderTreeNode {
            if let existing = nodesByPath[entry.path] {
                existing.kind = .entry(entry)
                existing.parentDiskSize = parentSize
                return existing
            }
            let node = FolderTreeNode(
                kind: .entry(entry), path: entry.path,
                parentPath: parentPath, parentDiskSize: parentSize
            )
            nodesByPath[entry.path] = node
            return node
        }

        private func synthetic(
            _ kind: FolderTreeNode.Kind, role: String, parentPath: String, parentSize: UInt64
        ) -> FolderTreeNode {
            let key = "\(parentPath)#\(role)"
            if let existing = syntheticNodes[key] {
                existing.kind = kind
                existing.parentDiskSize = parentSize
                return existing
            }
            let node = FolderTreeNode(
                kind: kind, path: key, parentPath: parentPath, parentDiskSize: parentSize
            )
            syntheticNodes[key] = node
            return node
        }

        /// The visible children of a directory, from the model's cache:
        /// loaded → sorted entries plus the residual leaf when the parent
        /// aggregate exceeds their sum; loading/nil → an inline "Loading…"
        /// row; failed → an inline retry row.
        private func childNodes(of parentPath: String, parentSize: UInt64) -> [FolderTreeNode] {
            switch model.childrenByPath[parentPath] {
            case .loaded(let entries):
                var nodes = sorted(entries).map {
                    node(for: $0, parentPath: parentPath, parentSize: parentSize)
                }
                let residual = ScansModel.residualSize(parent: parentSize, children: entries)
                if residual > 0 {
                    nodes.append(synthetic(
                        .residual(residual), role: "residual",
                        parentPath: parentPath, parentSize: parentSize
                    ))
                }
                return nodes
            case .failed(let message):
                return [synthetic(
                    .failure(message), role: "failure",
                    parentPath: parentPath, parentSize: parentSize
                )]
            case .loading, nil:
                return [synthetic(
                    .loading, role: "loading",
                    parentPath: parentPath, parentSize: parentSize
                )]
            }
        }

        /// The model's cache is canonically disk-descending; other column
        /// sorts are a per-view concern applied here. The residual row is
        /// appended after sorting — it is always last.
        private func sorted(_ entries: [ScanEntry]) -> [ScanEntry] {
            let sorted: [ScanEntry]
            switch sortKey {
            case "name":
                sorted = entries.sorted {
                    $0.name.localizedStandardCompare($1.name) == .orderedAscending
                }
            case "logical":
                sorted = entries.sorted { $0.logicalSize < $1.logicalSize }
            case "modified":
                sorted = entries.sorted {
                    ($0.modifiedAt ?? .distantPast) < ($1.modifiedAt ?? .distantPast)
                }
            default: // disk
                sorted = entries.sorted { $0.displaySize < $1.displaySize }
            }
            return sortAscending ? sorted : sorted.reversed()
        }

        private var rootPath: String? { model.selectedScan?.rootPath }
        private var rootSize: UInt64 { model.selectedScan?.totalDiskSize ?? 0 }

        // MARK: NSOutlineViewDataSource

        func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
            guard let node = item as? FolderTreeNode else {
                guard let rootPath else { return 0 }
                return childNodes(of: rootPath, parentSize: rootSize).count
            }
            guard let entry = node.entry, entry.isDir else { return 0 }
            return childNodes(of: entry.path, parentSize: entry.displaySize).count
        }

        func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
            if let node = item as? FolderTreeNode, let entry = node.entry {
                return childNodes(of: entry.path, parentSize: entry.displaySize)[index]
            }
            return childNodes(of: rootPath ?? "", parentSize: rootSize)[index]
        }

        func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
            (item as? FolderTreeNode)?.entry?.isDir == true
        }

        func outlineView(
            _ outlineView: NSOutlineView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]
        ) {
            guard let descriptor = outlineView.sortDescriptors.first, let key = descriptor.key else { return }
            sortKey = key
            sortAscending = descriptor.ascending
            reloadPreservingExpansion()
        }

        // MARK: lazy fetch + retry + re-root

        func outlineViewItemWillExpand(_ notification: Notification) {
            guard let node = notification.userInfo?["NSObject"] as? FolderTreeNode,
                  let entry = node.entry, entry.isDir,
                  model.childrenByPath[entry.path] == nil else { return }
            let path = entry.path
            Task { await model.loadChildren(path: path) }
        }

        /// Single mouse click: re-root the treemap at the clicked directory
        /// so the map reframes to follow the tree. Files and synthetic rows
        /// do nothing here (selection still inspects them). `clickedRow` is
        /// the row under the cursor; disclosure-triangle clicks don't fire
        /// the table action, so expanding a row never re-roots.
        @objc func onSingleClick(_ sender: Any?) {
            guard let outline, outline.clickedRow >= 0,
                  let node = outline.item(atRow: outline.clickedRow) as? FolderTreeNode,
                  case .entry(let entry) = node.kind, entry.isDir else { return }
            // A deliberate click re-roots NOW; cancel the settle timer the
            // selection change just armed so it doesn't fire a second time.
            reRootDebounce?.cancel()
            Task { await model.reRoot(to: entry.path) }
        }

        @objc func onDoubleClick(_ sender: Any?) {
            guard let outline, outline.clickedRow >= 0,
                  let node = outline.item(atRow: outline.clickedRow) as? FolderTreeNode else { return }
            // Directory re-root is handled by the single-click action above;
            // double-click only retries a failed children fetch.
            if case .failure = node.kind {
                Task { await model.retryChildren(path: node.parentPath) }
            }
        }

        func selectedEntryPath() -> String? {
            guard let outline else { return nil }
            return (outline.item(atRow: outline.selectedRow) as? FolderTreeNode)?.entry?.path
        }

        /// Row context menu: rebuilt per right-click for the clicked row.
        /// Reveal + Copy Path only — Phantom never deletes.
        func menuNeedsUpdate(_ menu: NSMenu) {
            menu.removeAllItems()
            guard let outline, outline.clickedRow >= 0,
                  let entry = (outline.item(atRow: outline.clickedRow) as? FolderTreeNode)?.entry
            else { return }
            let path = entry.path
            menu.addItem(withTitle: "Reveal in Finder", action: #selector(revealClicked(_:)), keyEquivalent: "")
                .target = self
            menu.addItem(withTitle: "Copy Path", action: #selector(copyPathClicked(_:)), keyEquivalent: "")
                .target = self
            menu.items.forEach { $0.representedObject = path }
        }

        @objc private func revealClicked(_ sender: NSMenuItem) {
            guard let path = sender.representedObject as? String else { return }
            FinderActions.reveal(path)
        }

        @objc private func copyPathClicked(_ sender: NSMenuItem) {
            guard let path = sender.representedObject as? String else { return }
            FinderActions.copyPath(path)
        }

        @objc func toggleLogicalColumn(_ sender: NSMenuItem) {
            guard let column = outline?.tableColumns.first(
                where: { $0.identifier.rawValue == "logical" }) else { return }
            column.isHidden = !column.isHidden
            sender.state = column.isHidden ? .off : .on
        }

        // MARK: selection

        func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
            // Synthetic rows (residual, loading, failure) are not
            // inspector-selectable.
            (item as? FolderTreeNode)?.entry != nil
        }

        func outlineViewSelectionDidChange(_ notification: Notification) {
            guard !isSyncingSelection, let outline else { return }
            let node = outline.item(atRow: outline.selectedRow) as? FolderTreeNode
            let path = node?.entry?.path
            guard path != model.selectedEntryPath else { return }
            Task { await model.selectEntry(path: path) }
            // Follow keyboard navigation too, but only after it settles: a
            // mouse click cancels this and re-roots immediately (onSingleClick),
            // so this timer is what remains for arrow-key moves.
            scheduleSettleReRoot(for: node)
        }

        /// (Re)arm the settle timer: if the selection still points at the same
        /// directory after `reRootSettleDelay`, re-root the map there. A new
        /// selection change cancels and re-arms it; a file selection cancels
        /// it without re-rooting (files never re-root).
        private func scheduleSettleReRoot(for node: FolderTreeNode?) {
            reRootDebounce?.cancel()
            guard let entry = node?.entry, entry.isDir else { return }
            let path = entry.path
            reRootDebounce = Task { [weak self] in
                try? await Task.sleep(for: self?.reRootSettleDelay ?? .milliseconds(350))
                guard let self, !Task.isCancelled else { return }
                // Skip if the settled folder is already the map's root.
                guard self.model.breadcrumbs.last != path else { return }
                await self.model.reRoot(to: path)
            }
        }

        /// Model → view selection sync (tree stays in step when the treemap
        /// or file list selects). Guarded so it never echoes back into
        /// selectEntry.
        func syncSelection(to path: String?) {
            guard let outline else { return }
            let row = path.flatMap { nodesByPath[$0] }.map { outline.row(forItem: $0) } ?? -1
            guard row != outline.selectedRow else { return }
            isSyncingSelection = true
            defer { isSyncingSelection = false }
            if row >= 0 {
                outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
            } else {
                outline.deselectAll(nil)
            }
        }

        /// Treemap → tree reveal: the model has already loaded every level
        /// of the chain; expand outermost-first, then scroll to and select
        /// the target row.
        func perform(reveal: ScansModel.TreeReveal) {
            guard let outline else { return }
            for ancestor in reveal.chain {
                // The scan root is the invisible top level — its children
                // ARE the root items; only deeper ancestors need expanding.
                guard ancestor != rootPath, let node = nodesByPath[ancestor] else { continue }
                outline.expandItem(node)
            }
            guard let target = nodesByPath[reveal.path] else { return }
            let row = outline.row(forItem: target)
            guard row >= 0 else { return }
            outline.scrollRowToVisible(row)
            syncSelection(to: reveal.path)
        }

        // MARK: cells

        func outlineView(
            _ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any
        ) -> NSView? {
            guard let node = item as? FolderTreeNode, let column = tableColumn else { return nil }
            switch column.identifier.rawValue {
            case "name": return nameCell(for: node, outlineView)
            case "items":
                let text: String
                if let entry = node.entry, entry.isDir,
                   let files = entry.fileCount, let dirs = entry.dirCount {
                    text = "\(files.formatted()) files, \(dirs.formatted()) dirs"
                } else {
                    // File rows, synthetic rows, and pre-v3 dir rows (the
                    // wire sends null) — never a client-side count, which
                    // would be a structural undercount.
                    text = "—"
                }
                return sizeCell(text: text, outlineView, id: "itemsCell", secondary: true, monospaced: false)
            case "percent": return percentCell(for: node, outlineView)
            case "disk":
                if case .loading = node.kind { return nil }
                if case .failure = node.kind { return nil }
                return sizeCell(text: Format.size(node.diskSize), outlineView, id: "diskCell")
            case "logical":
                guard let entry = node.entry else {
                    return sizeCell(text: "—", outlineView, id: "logicalCell", secondary: true)
                }
                return sizeCell(text: Format.size(entry.logicalSize), outlineView, id: "logicalCell", secondary: true)
            case "modified":
                let text = node.entry?.modifiedAt
                    .map { $0.formatted(date: .abbreviated, time: .omitted) } ?? "—"
                return sizeCell(text: text, outlineView, id: "modifiedCell", secondary: true, monospaced: false)
            default:
                return nil
            }
        }

        private func nameCell(for node: FolderTreeNode, _ outlineView: NSOutlineView) -> NSView {
            let id = NSUserInterfaceItemIdentifier("nameCell")
            let cell = outlineView.makeView(withIdentifier: id, owner: nil) as? NameCellView
                ?? NameCellView(identifier: id)
            // The swatch wears the per-scan legend color — the same mapping
            // as the treemap tiles and legend chips (dirs use the accent).
            let swatchColor: NSColor? = node.entry.map { entry in
                entry.isDir
                    ? NSColor(Palette.accent)
                    : NSColor(Palette.legendColor(slot: model.legendSlot(for: entry.fileType)))
            }
            cell.configure(with: node, swatchColor: swatchColor)
            return cell
        }

        private func percentCell(for node: FolderTreeNode, _ outlineView: NSOutlineView) -> NSView? {
            switch node.kind {
            case .loading, .failure: return nil
            default: break
            }
            let id = NSUserInterfaceItemIdentifier("percentCell")
            let cell = outlineView.makeView(withIdentifier: id, owner: nil) as? PercentCellView
                ?? PercentCellView(identifier: id)
            let fraction = node.parentDiskSize > 0
                ? Double(node.diskSize) / Double(node.parentDiskSize) : 0
            cell.configure(fraction: fraction)
            return cell
        }

        private func sizeCell(
            text: String, _ outlineView: NSOutlineView, id: String,
            secondary: Bool = false, monospaced: Bool = true
        ) -> NSView {
            let identifier = NSUserInterfaceItemIdentifier(id)
            let cell = outlineView.makeView(withIdentifier: identifier, owner: nil) as? TextCellView
                ?? TextCellView(identifier: identifier)
            cell.configure(text: text, secondary: secondary, monospaced: monospaced)
            return cell
        }
    }
}

// MARK: - Cell views

/// Name column: type swatch + name. Synthetic rows render secondary text
/// and NO swatch (the residual row is policy, not a file).
final class NameCellView: NSTableCellView {
    private let swatch = NSView()
    private let label = NSTextField(labelWithString: "")
    /// Reclaimability dot (spec SHOULD-11): shown only when the entry
    /// carries a classifier category; the tooltip names it.
    private let categoryDot = NSView()

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: .zero)
        self.identifier = identifier
        swatch.wantsLayer = true
        swatch.layer?.cornerRadius = TreeStyle.swatchSize / 2
        swatch.translatesAutoresizingMaskIntoConstraints = false
        categoryDot.wantsLayer = true
        categoryDot.layer?.cornerRadius = TreeStyle.swatchSize / 2
        categoryDot.translatesAutoresizingMaskIntoConstraints = false
        label.translatesAutoresizingMaskIntoConstraints = false
        label.lineBreakMode = .byTruncatingMiddle
        label.setContentHuggingPriority(.defaultLow, for: .horizontal)
        addSubview(swatch)
        addSubview(label)
        addSubview(categoryDot)
        NSLayoutConstraint.activate([
            swatch.leadingAnchor.constraint(equalTo: leadingAnchor),
            swatch.centerYAnchor.constraint(equalTo: centerYAnchor),
            swatch.widthAnchor.constraint(equalToConstant: TreeStyle.swatchSize),
            swatch.heightAnchor.constraint(equalToConstant: TreeStyle.swatchSize),
            label.leadingAnchor.constraint(equalTo: swatch.trailingAnchor, constant: Spacing.xs),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
            categoryDot.leadingAnchor.constraint(equalTo: label.trailingAnchor, constant: Spacing.xs),
            categoryDot.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
            categoryDot.centerYAnchor.constraint(equalTo: centerYAnchor),
            categoryDot.widthAnchor.constraint(equalToConstant: TreeStyle.swatchSize),
            categoryDot.heightAnchor.constraint(equalToConstant: TreeStyle.swatchSize),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    func configure(with node: FolderTreeNode, swatchColor: NSColor?) {
        categoryDot.isHidden = true
        toolTip = nil
        switch node.kind {
        case .entry(let entry):
            swatch.isHidden = false
            swatch.layer?.backgroundColor = (swatchColor ?? .clear).cgColor
            label.stringValue = entry.name
            label.textColor = .labelColor
            label.font = .systemFont(ofSize: NSFont.systemFontSize)
            if let category = entry.category {
                categoryDot.isHidden = false
                categoryDot.layer?.backgroundColor =
                    NSColor(Palette.categoryColor(for: category)).cgColor
                toolTip = category
            }
        case .residual:
            // Size lives in the Disk column like every other row; the name
            // column names the mechanism only.
            swatch.isHidden = true
            label.stringValue = "(smaller files)"
            label.textColor = .secondaryLabelColor
            label.font = .systemFont(ofSize: NSFont.systemFontSize)
        case .loading:
            swatch.isHidden = true
            label.stringValue = "Loading…"
            label.textColor = .secondaryLabelColor
            label.font = .systemFont(ofSize: NSFont.systemFontSize)
        case .failure(let message):
            swatch.isHidden = true
            label.stringValue = "Load failed (\(message)) — double-click to retry"
            label.textColor = NSColor(Palette.error)
            label.font = .systemFont(ofSize: NSFont.systemFontSize)
        }
    }
}

/// NSOutlineView with Finder's Space-for-Quick-Look. The closure returns
/// whether the key was consumed; everything else stays native.
final class QuickLookOutlineView: NSOutlineView {
    var onSpace: (() -> Bool)?

    override func keyDown(with event: NSEvent) {
        if event.charactersIgnoringModifiers == " ", onSpace?() == true {
            return
        }
        super.keyDown(with: event)
    }
}

/// % of Parent column: a bar scaled to the fraction of the PARENT's
/// aggregate (never sibling-normalized) plus the same number as text.
final class PercentCellView: NSTableCellView {
    private let bar = PercentBarView()
    private let label = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: .zero)
        self.identifier = identifier
        bar.translatesAutoresizingMaskIntoConstraints = false
        label.translatesAutoresizingMaskIntoConstraints = false
        label.font = .monospacedDigitSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        label.textColor = .secondaryLabelColor
        label.alignment = .right
        addSubview(bar)
        addSubview(label)
        NSLayoutConstraint.activate([
            bar.leadingAnchor.constraint(equalTo: leadingAnchor),
            bar.centerYAnchor.constraint(equalTo: centerYAnchor),
            bar.widthAnchor.constraint(equalToConstant: TreeStyle.barWidth),
            bar.heightAnchor.constraint(equalToConstant: TreeStyle.barHeight),
            label.leadingAnchor.constraint(equalTo: bar.trailingAnchor, constant: Spacing.xs),
            label.widthAnchor.constraint(equalToConstant: TreeStyle.percentLabelWidth),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    func configure(fraction: Double) {
        bar.fraction = max(0, min(1, fraction))
        label.stringValue = "\(Int((fraction * 100).rounded()))%"
    }
}

final class PercentBarView: NSView {
    var fraction: Double = 0 {
        didSet { needsDisplay = true }
    }

    override func draw(_ dirtyRect: NSRect) {
        let radius = TreeStyle.barHeight / 2
        let track = NSBezierPath(roundedRect: bounds, xRadius: radius, yRadius: radius)
        NSColor(Palette.accent).withAlphaComponent(TreeStyle.barTrackOpacity).setFill()
        track.fill()
        var fillRect = bounds
        fillRect.size.width = bounds.width * CGFloat(fraction)
        let fill = NSBezierPath(roundedRect: fillRect, xRadius: radius, yRadius: radius)
        NSColor(Palette.accent).withAlphaComponent(TreeStyle.barFillOpacity).setFill()
        fill.fill()
    }
}

/// Plain text cell (sizes, dates), right-aligned monospaced digits for the
/// size columns.
final class TextCellView: NSTableCellView {
    private let label = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: .zero)
        self.identifier = identifier
        label.translatesAutoresizingMaskIntoConstraints = false
        label.lineBreakMode = .byTruncatingTail
        addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: leadingAnchor),
            label.trailingAnchor.constraint(equalTo: trailingAnchor),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    func configure(text: String, secondary: Bool, monospaced: Bool) {
        label.stringValue = text
        label.textColor = secondary ? .secondaryLabelColor : .labelColor
        label.alignment = monospaced ? .right : .left
        label.font = monospaced
            ? .monospacedDigitSystemFont(ofSize: NSFont.systemFontSize, weight: .regular)
            : .systemFont(ofSize: NSFont.smallSystemFontSize)
    }
}
