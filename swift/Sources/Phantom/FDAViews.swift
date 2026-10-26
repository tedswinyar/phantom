// Full Disk Access guidance surfaces. NON-MODAL by design: scans that need
// no grant (~/Code) are legitimate, so nothing here blocks, nags on launch,
// or interrupts — a callout on affected scans, a quiet indicator while the
// grant is missing, and a menu item so the flow stays findable later.

import SwiftUI
import DesignKit
import PhantomCore

/// The scan-detail callout (phantom-di7). One control row: the count, a
/// Details… button whose popover lists the recorded paths by cause with a
/// next step each, Rescan, and — ONLY when the probe proved the grant
/// missing — Open System Settings. The wording never promises that Full
/// Disk Access fixes everything; the popover says what each bucket is.
struct ScanErrorsCallout: View {
    @Environment(ScansModel.self) private var model
    let scan: Scan
    @State private var showDetails = false

    var body: some View {
        if scan.errorCount > 0 {
            let fdaMissing = model.shouldShowFDAHint(for: scan)
            ScanErrorsCalloutRow(
                count: scan.errorCount,
                fdaMissing: fdaMissing,
                showDetails: $showDetails,
                onSettings: { FinderActions.openFullDiskAccessSettings() },
                onRescan: { Task { await model.startScan(rootPath: scan.rootPath) } }
            ) {
                UnreadableItemsList(
                    errorCount: scan.errorCount,
                    sample: scan.unreadablePaths,
                    fdaMissing: fdaMissing,
                    onSettings: { FinderActions.openFullDiskAccessSettings() },
                    onRescan: {
                        showDetails = false
                        Task { await model.startScan(rootPath: scan.rootPath) }
                    }
                )
            }
        }
    }
}

/// The callout's row, model-free so the layout gate can host it in both
/// states: it must stay ONE control tall at the detail column's floor width
/// (the label truncates rather than wraps) with every control inside.
struct ScanErrorsCalloutRow<Details: View>: View {
    let count: UInt64
    let fdaMissing: Bool
    @Binding var showDetails: Bool
    let onSettings: () -> Void
    let onRescan: () -> Void
    @ViewBuilder let details: () -> Details

    var body: some View {
        HStack(spacing: Spacing.sm) {
            Image(systemName: "eye.slash")
                .foregroundStyle(Palette.warning)
                .accessibilityHidden(true)
            Text(fdaMissing
                 ? "\(Self.items(count)) could not be read — Full Disk Access is missing."
                 : "\(Self.items(count)) could not be read.")
                .font(Typography.caption)
                .foregroundStyle(fdaMissing ? Palette.warning : Palette.textSecondary)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer(minLength: Spacing.sm)
            if fdaMissing {
                Button("Open System Settings", action: onSettings)
                    .help("System Settings → Privacy & Security → Full Disk Access; relaunch Phantom after granting")
            }
            Button("Details…") { showDetails = true }
                .help("Which items, why, and what to do about each kind")
                .popover(isPresented: $showDetails, arrowEdge: .bottom) {
                    details()
                        .frame(width: Size.popoverWidth)
                }
            Button("Rescan", action: onRescan)
                .help("Begin a new \(Vocabulary.scan.lowercased()) of the same folder")
        }
        .controlSize(.small)
        .padding(.horizontal, Spacing.md)
        .padding(.vertical, Spacing.xs)
        .background(Palette.warning.opacity(0.1))
        .overlay(alignment: .bottom) {
            Rectangle().fill(Palette.separator).frame(height: 1)
        }
    }

    static func items(_ count: UInt64) -> String {
        count == 1 ? "1 item" : "\(count.formatted()) items"
    }
}

/// The popover: the recorded sample grouped by cause, one next-step
/// sentence per group, each path with its OS reason on hover and Reveal /
/// Copy in the context menu. Model-free; hosted by the layout gate at
/// `Size.popoverWidth` with a full 100-path sample.
struct UnreadableItemsList: View {
    let errorCount: UInt64
    let sample: [UnreadablePath]?
    let fdaMissing: Bool
    let onSettings: () -> Void
    let onRescan: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: Spacing.sm) {
            Text("\(ScanErrorsCalloutRow<EmptyView>.items(errorCount)) could not be read")
                .font(Typography.title)
            if let sample {
                if sample.isEmpty {
                    Text("The \(Vocabulary.scan.lowercased()) counted them but recorded no paths; rescan to see which items.")
                        .font(Typography.caption)
                        .foregroundStyle(Palette.textSecondary)
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: Spacing.md) {
                            ForEach(UnreadableGroup.group(sample), id: \.cause) { group in
                                groupSection(group)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: Size.popoverMaxHeight)
                    if errorCount > UInt64(sample.count) {
                        Text("Showing the first \(sample.count) of \(errorCount.formatted()); the \(Vocabulary.scan.lowercased()) keeps a sample.")
                            .font(Typography.caption)
                            .foregroundStyle(Palette.textSecondary)
                    }
                }
            } else {
                Text("This \(Vocabulary.scan.lowercased()) predates path recording. Rescan to see which items and why.")
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
            }
            HStack {
                if fdaMissing {
                    Button("Open System Settings", action: onSettings)
                }
                Spacer()
                Button("Rescan", action: onRescan)
                    .buttonStyle(.borderedProminent)
            }
            .controlSize(.small)
        }
        .padding(Spacing.md)
    }

    private func groupSection(_ group: UnreadableGroup) -> some View {
        VStack(alignment: .leading, spacing: Spacing.xs) {
            Text("\(group.paths.count) \(group.cause.title)")
                .font(Typography.body.weight(.semibold))
            Text(group.cause.nextStep(fdaMissing: fdaMissing))
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
                .fixedSize(horizontal: false, vertical: true)
            ForEach(group.paths, id: \.path) { item in
                Text(item.path)
                    .font(Typography.monoCaption)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(item.reason)
                    .contextMenu {
                        Button("Reveal in Finder") { FinderActions.reveal(item.path) }
                        Button("Copy Path") { FinderActions.copyPath(item.path) }
                    }
            }
        }
    }
}

// Copy reads Vocabulary (main-actor state), so the extension is too; every
// caller is a View body.
@MainActor
extension UnreadablePath.Cause {
    /// Reads after the count: "3 protected by macOS".
    var title: String {
        switch self {
        case .protected: "protected by macOS"
        case .transient: "changed or vanished during the \(Vocabulary.scan.lowercased())"
        case .timedOut: "timed out"
        case .other: "failed for another reason"
        }
    }

    /// One sentence of what to do. Honest about what a grant can and cannot
    /// fix: with the grant present, a refusal is something else.
    func nextStep(fdaMissing: Bool) -> String {
        switch self {
        case .protected:
            fdaMissing
                ? "Grant Phantom Full Disk Access in System Settings, then rescan."
                : "Phantom has Full Disk Access, so either the grant was missing when this \(Vocabulary.scan.lowercased()) ran or these belong to another user or an app's sandbox. Rescan once to tell which; nothing is counted for them."
        case .transient:
            "A running app was writing here as the \(Vocabulary.scan.lowercased()) passed (temporary files, caches). Harmless; rescan if the folder matters."
        case .timedOut:
            "A cloud or network location did not answer in time (OneDrive, iCloud, a mounted share). Its placeholders take no local space; rescan with the provider online if you need them."
        case .other:
            "Not a permission problem. Hover a path for the exact reason from macOS; repeated read errors on a local disk are worth a Disk Utility check."
        }
    }
}

/// The quiet indicator: one line, secondary styling, shown wherever the
/// missing grant is about to matter (scan progress) or should stay gently
/// visible (sidebar footer). Renders nothing unless the probe PROVED the
/// grant missing.
struct FDAQuietIndicator: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        if model.fdaStatus == .notGranted {
            HStack(spacing: Spacing.xs) {
                Image(systemName: "lock.shield")
                    .foregroundStyle(Palette.textSecondary)
                    .accessibilityHidden(true)
                Text("No Full Disk Access — protected folders are skipped")
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
                Button("Grant…") {
                    FinderActions.openFullDiskAccessSettings()
                }
                .buttonStyle(.link)
                .font(Typography.caption)
            }
            .padding(.horizontal, Spacing.md)
            .padding(.vertical, Spacing.xs)
        }
    }
}
