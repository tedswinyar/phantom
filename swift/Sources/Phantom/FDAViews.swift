// Full Disk Access guidance surfaces. NON-MODAL by design: scans that need
// no grant (~/Code) are legitimate, so nothing here blocks, nags on launch,
// or interrupts — a callout on affected scans, a quiet indicator while the
// grant is missing, and a menu item so the flow stays findable later.

import SwiftUI
import DesignKit
import PhantomCore

/// The scan-detail callout. Two honesty levels: any scan with walk errors
/// shows its unreadable-item count (errors have many causes); the FDA hint
/// and settings button appear ONLY when the probe proved the grant missing
/// — and even then the wording is "often caused by", never a promise.
struct ScanErrorsCallout: View {
    @Environment(ScansModel.self) private var model
    let scan: Scan

    var body: some View {
        if scan.errorCount > 0 {
            HStack(spacing: Spacing.sm) {
                Image(systemName: "eye.slash")
                    .foregroundStyle(Palette.warning)
                    .accessibilityHidden(true)
                if model.shouldShowFDAHint(for: scan) {
                    Text("\(scan.errorCount.formatted()) items could not be read — often caused by missing Full Disk Access. Grant it for a complete picture.")
                        .font(Typography.caption)
                    Button("Open System Settings") {
                        FinderActions.openFullDiskAccessSettings()
                    }
                    .font(Typography.caption)
                    .help("System Settings → Privacy & Security → Full Disk Access; relaunch Phantom after granting")
                } else {
                    Text("\(scan.errorCount.formatted()) items could not be read.")
                        .font(Typography.caption)
                        .foregroundStyle(Palette.textSecondary)
                }
                Spacer()
            }
            .padding(.horizontal, Spacing.md)
            .padding(.vertical, Spacing.xs)
            .background(Palette.warning.opacity(0.1))
            .overlay(alignment: .bottom) {
                Rectangle().fill(Palette.separator).frame(height: 1)
            }
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
