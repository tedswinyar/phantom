// Sidebar: the list of scans (Crypts). Selecting one drives every detail
// surface through ScansModel.select. A running scan's row shows the live
// poll counters; terminal rows show the headline disk size and a status
// badge that renders complete/cancelled/failed distinctly.

import SwiftUI
import DesignKit
import PhantomCore

struct SidebarView: View {
    @Environment(ScansModel.self) private var model

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
                        ScanRow(scan: scan)
                            .tag(scan.id)
                    }
                }
                .listStyle(.sidebar)
            }
        }
        .navigationTitle(Vocabulary.sidebarTitle)
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

    var body: some View {
        HStack(spacing: Spacing.sm) {
            statusGlyph
                .accessibilityHidden(true) // status is in the composed label
            VStack(alignment: .leading, spacing: Spacing.xs) {
                Text(displayName)
                    .font(Typography.rowTitle)
                    .lineLimit(1)
                Text(subtitle)
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
                    .lineLimit(1)
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
