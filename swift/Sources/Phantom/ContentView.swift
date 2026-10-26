// The app shell: sidebar of scans (Crypts), detail per the selected scan's
// lifecycle — live progress with cancel while running, honest terminal
// states for cancelled/failed, and the treemap + file list + inspector for
// a complete scan. DesignKit tokens only; product terms from Vocabulary;
// errors always surfaced.

import SwiftUI
import DesignKit
import PhantomCore

struct ContentView: View {
    @Environment(ScansModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        @Bindable var model = model
        Group {
            switch model.connectionState {
            case .connecting:
                ProgressView(Vocabulary.connecting)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .failed(let message):
                connectionError(message)
            case .connected:
                NavigationSplitView {
                    SidebarView()
                        // The band is a term of the window floor
                        // (Size.windowMinWidth); an unpinned sidebar can be
                        // dragged wider than the floor accounts for.
                        .navigationSplitViewColumnWidth(
                            min: Size.sidebarMinWidth,
                            ideal: Size.sidebarIdealWidth,
                            max: Size.sidebarMaxWidth
                        )
                } detail: {
                    detail
                }
            }
        }
        // Floors, not fixed sizes — content taller than the floor grows the
        // window rather than clipping.
        .frame(minWidth: Size.windowMinWidth, minHeight: Size.windowMinHeight)
        .navigationTitle(Vocabulary.appName)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                // A plain plus: the universal "new" affordance. The prior
                // eye-with-badge glyph read as a WARNING, and once launch
                // auto-selects a scan the empty state's labeled button never
                // shows — this icon was the only way to start an analysis.
                Button {
                    model.showScanSheet = true
                } label: {
                    Label("New \(Vocabulary.scan)", systemImage: "plus")
                }
                .help("Begin a new \(Vocabulary.scan.lowercased()) — analyze another folder")
                .disabled(model.connectionState != .connected)
            }
            ToolbarItem {
                Button {
                    withAnimation { model.showInspector.toggle() }
                } label: {
                    Label(Vocabulary.inspector, systemImage: "sidebar.trailing")
                }
                .help("Toggle the \(Vocabulary.inspector.lowercased())")
            }
        }
        .sheet(isPresented: $model.showScanSheet) {
            ScanSheet()
        }
        // Coming back from System Settings lands here: re-probe so the
        // indicator and callouts update without a relaunch prompt.
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { model.refreshFDAStatus() }
        }
    }

    // MARK: - Detail (per scan lifecycle)

    @ViewBuilder private var detail: some View {
        VStack(spacing: 0) {
            if let error = model.lastError {
                errorBanner(error)
            }
            if let scan = model.selectedScan {
                switch scan.status {
                case .running:
                    ScanProgressPanel(scan: scan)
                case .complete:
                    ScanErrorsCallout(scan: scan)
                    completeDetail
                case .cancelled:
                    terminalState(
                        icon: "moon.zzz",
                        headline: "\(Vocabulary.scan) cancelled",
                        detail: "Partial results are discarded; begin a new \(Vocabulary.scan.lowercased()) to see this \(Vocabulary.volume.lowercased())."
                    )
                case .failed:
                    // The server's reason when it has one (a walk error, or
                    // "interrupted: …" for a scan a previous server process
                    // was running when it stopped); the generic line otherwise.
                    terminalState(
                        icon: "exclamationmark.triangle",
                        headline: "\(Vocabulary.scan) failed",
                        detail: scan.failureReason
                            ?? "The walk could not finish. Check the server log, then begin a new \(Vocabulary.scan.lowercased())."
                    )
                }
            } else {
                noSelectionState
            }
        }
    }

    private var completeDetail: some View { CompleteScanDetail() }

    private var noSelectionState: some View {
        VStack(spacing: Spacing.md) {
            Image(systemName: "rectangle.split.3x3")
                .font(.system(size: Size.iconHero))
                .foregroundStyle(Palette.textSecondary)
                .accessibilityHidden(true)
            Text(Vocabulary.noSelection)
                .font(Typography.body)
                .foregroundStyle(Palette.textSecondary)
            Button("New \(Vocabulary.scan)") { model.showScanSheet = true }
                .buttonStyle(.borderedProminent)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func terminalState(icon: String, headline: String, detail: String) -> some View {
        VStack(spacing: Spacing.md) {
            Image(systemName: icon)
                .font(.system(size: Size.iconHero))
                .foregroundStyle(Palette.textSecondary)
                .accessibilityHidden(true)
            Text(headline)
                .font(Typography.title)
            Text(detail)
                .font(Typography.body)
                .foregroundStyle(Palette.textSecondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, Spacing.xl)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func errorBanner(_ message: String) -> some View {
        Text(message)
            .font(Typography.caption)
            .foregroundStyle(Palette.error)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(Spacing.sm)
            .background(Palette.error.opacity(0.1))
            .overlay(alignment: .bottom) {
                Rectangle().fill(Palette.separator).frame(height: 1)
            }
    }

    private func connectionError(_ message: String) -> some View {
        VStack(spacing: Spacing.md) {
            Image(systemName: "bolt.horizontal.circle")
                .font(.largeTitle)
                .foregroundStyle(Palette.error)
                .accessibilityHidden(true) // decorative; the message carries the meaning
            Text(message)
                .font(Typography.body)
                .multilineTextAlignment(.center)
                .padding(.horizontal, Spacing.xl)
            Button("Retry") {
                Task { await model.connect() }
            }
            .buttonStyle(.borderedProminent)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// The detail for a COMPLETE scan: treemap over the lower tabs, inspector
/// beside them. Its own type (not a `ContentView` computed property) so the
/// layout gate can host it at window size (`WindowFloorLayoutTests`); it
/// still reads `ScansModel` from the environment like every view here.
struct CompleteScanDetail: View {
    @Environment(ScansModel.self) private var model
    /// The inspector width the user last dragged to; read through
    /// `InspectorWidth.clamped` so the stored number can never exceed the
    /// band the window floor accounts for.
    @AppStorage(InspectorWidth.storageKey) private var storedInspectorWidth: Double = Size.inspectorDefaultWidth

    var body: some View {
        // An HStack, NOT an HSplitView. Linked against the macOS 26 SDK, the
        // VSplitView inside an HSplitView was observed to loop in windows
        // under ~560pt tall (NSGenericException "more Update Constraints in
        // Window passes than there are views" on every launch at the 900x532
        // default once a scan is shown); removing the HSplitView stops it.
        // `.inspector` crashed the same way. The cause inside SwiftUI is not
        // identified, and it is SDK-gated, so recheck on an SDK bump.
        // Resizing comes from InspectorResizeHandle (pure SwiftUI), not from
        // a split container.
        HStack(spacing: 0) {
            treemapAndList
            if model.showInspector {
                InspectorResizeHandle(width: $storedInspectorWidth)
                InspectorView()
                    .frame(width: InspectorWidth.clamped(storedInspectorWidth))
            }
        }
    }

    private var treemapAndList: some View {
        VSplitView {
            VStack(spacing: 0) {
                BreadcrumbBar()
                TreemapView()
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .frame(minHeight: Size.treemapMinHeight)

            LowerTabPane()
                .frame(
                    minHeight: Size.fileListMinHeight,
                    idealHeight: Size.fileListIdealHeight
                )
        }
        // Escape backs out one treemap drill level (cancelOperation:
        // bubbles here from AppKit); Cmd-Up does the same via the Go
        // menu command in PhantomApp.
        .onExitCommand {
            Task { await model.drillOut() }
        }
        // The floor SwiftUI cannot see on its own (the split view is AppKit
        // and reports no minimum); it makes the window's minimum size honest
        // — see Size.detailMinWidth.
        .frame(minWidth: Size.detailMinWidth)
    }
}

/// Live progress for a running scan: the poll's counters, updated as
/// ScansModel upserts each snapshot, with cooperative cancel.
struct ScanProgressPanel: View {
    @Environment(ScansModel.self) private var model
    let scan: Scan

    var body: some View {
        VStack(spacing: Spacing.md) {
            ProgressView()
                .controlSize(.large)
            Text("\(Vocabulary.scanning) \(scan.rootPath)")
                .font(Typography.title)
            if let progress = scan.progress {
                VStack(spacing: Spacing.xs) {
                    Text("\(progress.filesSeen) files seen — \(Format.size(progress.bytesSeen))")
                        .font(Typography.body)
                        .monospacedDigit()
                    Text(progress.currentPath)
                        .font(Typography.mono)
                        .foregroundStyle(Palette.textSecondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .padding(.horizontal, Spacing.xl)
                }
            }
            Button("Cancel", role: .destructive) {
                Task { await model.cancel(id: scan.id) }
            }
            .help("Stop this \(Vocabulary.scan.lowercased()); partial results are discarded")
            FDAQuietIndicator()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// Path entry + folder picker for a new scan (Haunt). POST answers 202 and
/// the sidebar row starts showing live progress immediately.
struct ScanSheet: View {
    @Environment(ScansModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var scanPath = ""

    var body: some View {
        VStack(alignment: .leading, spacing: Spacing.md) {
            Text("Begin \(Vocabulary.scan)")
                .font(Typography.title)

            TextField("Path to \(Vocabulary.scan.lowercased())", text: $scanPath)
                .textFieldStyle(.roundedBorder)
                .onSubmit { startScan() }

            HStack {
                Button("Choose Folder…") {
                    let panel = NSOpenPanel()
                    panel.canChooseFiles = false
                    panel.canChooseDirectories = true
                    panel.allowsMultipleSelection = false
                    if panel.runModal() == .OK, let url = panel.url {
                        scanPath = url.path
                    }
                }
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(Vocabulary.scan) { startScan() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(scanPath.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(Spacing.lg)
        // Flexible width (not a fixed 420) so the sheet reflows at large
        // Dynamic Type sizes instead of clipping (a11y H / design L3).
        .frame(
            minWidth: Size.sheetMinWidth,
            idealWidth: Size.sheetIdealWidth,
            maxWidth: Size.sheetMaxWidth
        )
    }

    private func startScan() {
        let path = scanPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !path.isEmpty else { return }
        dismiss()
        Task {
            await model.startScan(rootPath: path)
        }
    }
}

/// The lower pane: ONE tabbed region [Folders | Largest Files | Reclaimable | History]
/// with the persistent reclaim-estimate chip in the tab row (the estimate is
/// the server's globally-deduped number, visible whichever tab is up).
struct LowerTabPane: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        @Bindable var model = model
        VStack(spacing: 0) {
            HStack(spacing: Spacing.md) {
                Picker("", selection: $model.lowerTab) {
                    ForEach(ScansModel.LowerTab.allCases, id: \.self) { tab in
                        Text(tab == .reclaimable ? Vocabulary.reclaimable : tab.rawValue)
                            .tag(tab)
                    }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()

                Spacer()

                if let summary = model.hotspots {
                    // The estimate yields before it wraps: the full phrase
                    // when the row has room, the bare number when it has
                    // less, nothing at the detail's floor (the Reclaimable
                    // tab carries it anyway). As a plain Text it wrapped to
                    // five lines beside the picker at 900pt (phantom-xb1);
                    // a control row never holds a wrapping label.
                    ViewThatFits(in: .horizontal) {
                        estimate("reclaim estimate: \(Format.size(summary.reclaimEstimate))")
                        estimate(Format.size(summary.reclaimEstimate))
                        Color.clear.frame(width: 0, height: 0)
                    }
                }
            }
            .padding(.horizontal, Spacing.md)
            .padding(.vertical, Spacing.xs)

            Rectangle()
                .fill(Palette.separator)
                .frame(height: 1)
                .accessibilityHidden(true)

            switch model.lowerTab {
            case .folders:
                FoldersView()
            case .largestFiles:
                FileListView()
            case .reclaimable:
                ReclaimableView()
            case .history:
                HistoryView()
            }
        }
    }

    private func estimate(_ text: String) -> some View {
        Text(text)
            .font(Typography.caption)
            .foregroundStyle(Palette.success)
            .lineLimit(1)
            .fixedSize()
            .help("Reclaim estimate: hardlink-deduped disk across the reclaimable categories; cloud placeholders excluded")
    }
}
