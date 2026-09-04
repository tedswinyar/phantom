// The reclaimable-space pane (Restless Spirits): the classifier's hotspot
// groups for a complete scan. Per group: the category, the action hint, the
// hardlink-DEDUPED disk size as the headline, and the biggest paths.
//
// Phantom never deletes. The only affordances here are REVEAL IN FINDER and
// COPY the hint's suggested command — no destructive control exists, not
// even disabled. Cloud-dataloaded groups render as informational: the wire
// already excludes them from the reclaim estimate, and this view never
// recomputes what the server summed.

import AppKit
import SwiftUI
import DesignKit
import PhantomCore

struct ReclaimableView: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        VStack(spacing: 0) {
            if let summary = model.hotspots {
                if summary.isEmpty {
                    emptyState
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: Spacing.sm) {
                            if summary.reviewDiskSize > 0 {
                                Text("review first: \(Format.size(summary.reviewDiskSize)) — visible, never suggested")
                                    .font(Typography.caption)
                                    .foregroundStyle(Palette.warning)
                            }
                            ForEach(summary.groups) { group in
                                HotspotGroupRow(group: group)
                            }
                            if summary.cloudDataloadedLogicalSize > 0 {
                                cloudFootnote(summary)
                            }
                        }
                        .padding(Spacing.md)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            } else {
                // Selection load hasn't landed (or answered 409 mid-run).
                Text("Totals appear once the \(Vocabulary.scan.lowercased()) finishes.")
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }

    private var emptyState: some View {
        Text("No \(Vocabulary.reclaimable.lowercased()) — nothing here needs putting to rest.")
            .font(Typography.caption)
            .foregroundStyle(Palette.textSecondary)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func cloudFootnote(_ summary: HotspotsSummary) -> some View {
        // The du-lie, spelled out once for the whole scan: placeholders are
        // information, not an opportunity.
        Text("Cloud placeholders claim \(Format.size(summary.cloudDataloadedLogicalSize)) but occupy \(Format.size(summary.cloudDataloadedDiskSize)) on disk — deleting them frees almost nothing.")
            .font(Typography.caption)
            .foregroundStyle(Palette.textSecondary)
            .padding(.top, Spacing.xs)
    }
}

struct HotspotGroupRow: View {
    let group: HotspotGroup

    var body: some View {
        VStack(alignment: .leading, spacing: Spacing.xs) {
            HStack(spacing: Spacing.sm) {
                Circle()
                    .fill(Palette.categoryColor(for: group.category))
                    .frame(width: Size.statusDot, height: Size.statusDot)
                    .accessibilityHidden(true) // category is in the composed label
                Text(group.label)
                    .font(Typography.rowTitle)
                if group.isCloudDataloaded {
                    badge("informational", color: Palette.info)
                } else if group.isReviewOnly {
                    badge("review first", color: Palette.warning)
                }
                Spacer(minLength: Spacing.sm)
                // The deduped physical size — THE number for this group.
                Text(Format.size(group.diskSize))
                    .font(Typography.rowTitle)
                    .monospacedDigit()
            }

            Text(group.hint)
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)

            HStack(spacing: Spacing.md) {
                Text(context)
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
                if let command = group.command {
                    // Informational: names the safe tool. The RUNNABLE,
                    // location-aware copy lives on each path row below (a
                    // bare `cargo clean` isn't actionable without its dir).
                    Text("safe tool: `\(command)`")
                        .font(Typography.caption)
                        .foregroundStyle(Palette.textSecondary)
                }
            }

            ForEach(group.topPaths, id: \.self) { path in
                HStack(spacing: Spacing.sm) {
                    Text(path)
                        .font(Typography.mono)
                        .foregroundStyle(Palette.textSecondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                    if let command = group.command {
                        // Paste-and-run: `cd '<project dir>' && <command>`.
                        // Phantom copies it; the user runs it.
                        Button {
                            FinderActions.copyRunnableCommand(command, forArtifactAt: path)
                        } label: {
                            Image(systemName: "doc.on.doc")
                                .font(Typography.caption)
                        }
                        .buttonStyle(.borderless)
                        .help("Copy `\(FinderActions.runnableCommand(command, forArtifactAt: path))` — Phantom never deletes; you run it")
                        .accessibilityLabel("Copy the runnable command for \(path)")
                    } else {
                        // No safe command exists (cleanup here is deletion,
                        // which Phantom never suggests) — copy the path so
                        // the user can act on it themselves.
                        Button {
                            FinderActions.copyPath(path)
                        } label: {
                            Image(systemName: "doc.on.doc")
                                .font(Typography.caption)
                        }
                        .buttonStyle(.borderless)
                        .help("Copy this path")
                        .accessibilityLabel("Copy the path \(path)")
                    }
                    Button {
                        FinderActions.reveal(path)
                    } label: {
                        Image(systemName: "magnifyingglass.circle")
                            .font(Typography.caption)
                    }
                    .buttonStyle(.borderless)
                    .help("Reveal in Finder")
                    .accessibilityLabel("Reveal \(path) in Finder")
                }
            }
        }
        .padding(Spacing.sm)
        .background(Palette.cardBackground)
        .clipShape(RoundedRectangle(cornerRadius: Radius.card))
        .accessibilityElement(children: .contain)
        .accessibilityLabel(
            "\(group.label), \(Format.size(group.diskSize)), category \(group.category)"
        )
    }

    /// Secondary facts, shown only when they add something: the hardlink
    /// listed-vs-freed gap, the cloud claimed-vs-occupied gap, file count.
    private var context: String {
        var parts = ["\(group.fileCount) files"]
        if group.listedDiskSize > group.diskSize {
            parts.append("lists as \(Format.size(group.listedDiskSize)) — hardlinks free less than they list")
        }
        if group.isCloudDataloaded {
            parts.append("claims \(Format.size(group.logicalSize)) in the cloud")
        }
        return parts.joined(separator: " · ")
    }

    private func badge(_ text: String, color: Color) -> some View {
        Text(text)
            .font(Typography.caption)
            .foregroundStyle(color)
            .padding(.horizontal, Spacing.xs)
            .background(color.opacity(0.15))
            .clipShape(RoundedRectangle(cornerRadius: Radius.control))
    }
}
