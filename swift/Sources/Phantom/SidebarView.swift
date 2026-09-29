// Sidebar: the list of scans (Crypts). Selecting one drives every detail
// surface through ScansModel.select. A running scan's row shows the live
// poll counters; terminal rows show the headline disk size and a status
// badge that renders complete/cancelled/failed distinctly.

import SwiftUI
import DesignKit
import PhantomCore

struct SidebarView: View {
    @Environment(ScansModel.self) private var model
    /// The scan a removal is being confirmed for; nil == no alert showing.
    /// View state, not model state: the question is transient UI, the
    /// answer (`model.remove`) is what persists.
    @State private var pendingRemoval: Scan?

    var body: some View {
        Group {
            if model.scans.isEmpty {
                emptyState
            } else {
                List(selection: Binding(
                    get: { model.selectedScanID },
                    set: { newID in Task { await model.select(scanID: newID) } }
                )) {
                    ForEach(model.scans) { scan in
                        ScanRow(scan: scan, requestRemoval: { pendingRemoval = $0 })
                            .tag(scan.id)
                    }
                }
                .listStyle(.sidebar)
                // Delete / Backspace on the focused sidebar row — the Mac
                // idiom for "remove this". A running scan is not removable
                // (the API answers 409) so the key does nothing for it; the
                // row's own cancel button is the affordance there.
                .onDeleteCommand {
                    if let scan = model.selectedScan, scan.isTerminal {
                        pendingRemoval = scan
                    }
                }
            }
        }
        .navigationTitle(Vocabulary.sidebarTitle)
        // One confirmation for every removal path (context menu, Delete
        // key). It says the one thing a user must know: files are untouched.
        .alert(
            "Remove this \(Vocabulary.scan.lowercased())?",
            isPresented: Binding(
                get: { pendingRemoval != nil },
                set: { if !$0 { pendingRemoval = nil } }
            ),
            presenting: pendingRemoval
        ) { scan in
            Button("Remove", role: .destructive) {
                Task { await model.remove(id: scan.id) }
            }
            Button("Cancel", role: .cancel) {}
        } message: { scan in
            Text("\(scan.rootPath)\n\nIts recorded results are deleted from Phantom. Nothing on disk is touched.")
        }
        // The sidebar's own add affordance: once launch auto-selects a scan
        // the big empty-state button never shows, so the list of Crypts
        // carries a labeled way to start the next analysis.
        .safeAreaInset(edge: .bottom) {
            VStack(alignment: .leading, spacing: 0) {
                // The quiet, always-there reminder while the Full Disk
                // Access grant is provably missing; renders nothing
                // otherwise.
                FDAQuietIndicator()
                Button {
                    model.showScanSheet = true
                } label: {
                    Label("New \(Vocabulary.scan)", systemImage: "plus")
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .buttonStyle(.borderless)
                .padding(.horizontal, Spacing.md)
                .padding(.vertical, Spacing.sm)
                .help("Begin a new \(Vocabulary.scan.lowercased()) — analyze another folder")
                .disabled(model.connectionState != .connected)
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: Spacing.md) {
            Image(systemName: "moon.stars")
                .font(.system(size: Size.iconLarge))
                .foregroundStyle(Palette.textSecondary)
                .accessibilityHidden(true) // decorative
            Text("No \(Vocabulary.sidebarTitle.lowercased()) yet. Begin a \(Vocabulary.scan.lowercased()).")
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(Spacing.md)
    }
}

struct ScanRow: View {
    @Environment(ScansModel.self) private var model
    let scan: Scan
    /// Asks the sidebar to confirm removing this scan (the alert lives on
    /// the List so one confirmation serves every entry point).
    var requestRemoval: (Scan) -> Void = { _ in }

    var body: some View {
        HStack(spacing: Spacing.sm) {
            statusGlyph
                .accessibilityHidden(true) // status is in the composed label
            VStack(alignment: .leading, spacing: Spacing.xs) {
                Text(displayName)
                    .font(Typography.rowTitle)
                    .lineLimit(1)
                // v1.1 Phase 4: the root's history at a glance, from the
                // scan list already loaded — nothing is fetched for it. Two
                // or more complete scans of this root draw a line; fewer draw
                // nothing. It sits on the subtitle line so the name keeps the
                // row's full width, and the TEXT wins the width contest: a
                // narrow sidebar drops the sparkline rather than truncating
                // the size (smoke 2026-09-08: "swin…", then "242.66…").
                let history = ScansModel.sparklineValues(root: scan.rootPath, in: model.scans)
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: Spacing.sm) {
                        subtitleText
                        if !history.isEmpty, scan.status == .complete {
                            Sparkline(values: history)
                                .frame(width: Size.sparklineWidth, height: Size.sparklineHeight)
                                .help("\(history.count) \(Vocabulary.scan.lowercased())s of this folder: \(Format.size(history.first ?? 0)) → \(Format.size(history.last ?? 0))")
                                .accessibilityLabel("History: \(Format.size(history.first ?? 0)) to \(Format.size(history.last ?? 0)) over \(history.count) scans")
                        }
                    }
                    subtitleText
                }
            }
            Spacer(minLength: Spacing.sm)
            if scan.status == .running {
                Button {
                    Task { await model.cancel(id: scan.id) }
                } label: {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.borderless)
                .help("Cancel this \(Vocabulary.scan.lowercased())")
                .accessibilityLabel("Cancel \(Vocabulary.scan) of \(displayName)")
            }
        }
        .padding(.vertical, Spacing.xs)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(statusLabel) \(Vocabulary.scan.lowercased()): \(displayName)")
        .accessibilityValue(subtitle)
        // Right-click: cancel while running, remove once terminal. The two
        // never appear together — the API forbids deleting a running scan,
        // so offering it would only produce a 409 banner.
        .contextMenu {
            if scan.status == .running {
                Button("Cancel \(Vocabulary.scan)") {
                    Task { await model.cancel(id: scan.id) }
                }
            } else {
                Button("Remove \(Vocabulary.scan)…", role: .destructive) {
                    requestRemoval(scan)
                }
            }
        }
    }

    /// The subtitle as a fixed-size text so ViewThatFits measures its real
    /// width instead of letting it truncate to make room.
    private var subtitleText: some View {
        Text(subtitle)
            .font(Typography.caption)
            .foregroundStyle(Palette.textSecondary)
            .lineLimit(1)
            .fixedSize(horizontal: true, vertical: false)
    }

    private var displayName: String {
        (scan.rootPath as NSString).lastPathComponent
    }

    /// Running rows carry the live counters; terminal rows the headline
    /// disk size (never the logical size) and the honest status word.
    private var subtitle: String {
        if let progress = scan.progress {
            return "\(Vocabulary.scanning) \(progress.filesSeen) files, \(Format.size(progress.bytesSeen))"
        }
        switch scan.status {
        case .complete:
            return Format.size(scan.totalDiskSize)
        case .cancelled, .failed, .running:
            return statusLabel
        }
    }

    private var statusLabel: String {
        switch scan.status {
        case .running: return Vocabulary.scanning
        case .complete: return "Complete"
        case .cancelled: return "Cancelled"
        case .failed: return "Failed"
        }
    }

    @ViewBuilder private var statusGlyph: some View {
        switch scan.status {
        case .running:
            ProgressView()
                .controlSize(.small)
        case .complete:
            Circle()
                .fill(Palette.success)
                .frame(width: Size.statusDot, height: Size.statusDot)
        case .cancelled:
            Circle()
                .fill(Palette.textSecondary)
                .frame(width: Size.statusDot, height: Size.statusDot)
        case .failed:
            Circle()
                .fill(Palette.error)
                .frame(width: Size.statusDot, height: Size.statusDot)
        }
    }
}
