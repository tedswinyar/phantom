// The inspector (Séance): details of the selected entry. Disk Usage is the
// headline size; Logical Size renders only when it differs, explicitly
// labeled as the secondary number (cloud-dataloaded files diverge wildly —
// the disk number is the truth about local blocks).

import SwiftUI
import DesignKit
import PhantomCore

struct InspectorView: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        Group {
            if let entry = model.selectedEntry {
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        HStack(spacing: Spacing.md) {
                            EntryIcon(entry: entry)
                                .frame(width: Size.iconLarge, height: Size.iconLarge)
                            VStack(alignment: .leading, spacing: Spacing.xs) {
                                Text(entry.name)
                                    .font(Typography.rowTitle)
                                    .lineLimit(2)
                                Text(entry.isDir ? "Directory" : kindDescription(for: entry))
                                    .font(Typography.caption)
                                    .foregroundStyle(Palette.textSecondary)
                            }
                        }
                        .padding(Spacing.md)

                        Rectangle()
                            .fill(Palette.separator)
                            .frame(height: 1)
                            .accessibilityHidden(true)

                        Grid(
                            alignment: .leadingFirstTextBaseline,
                            horizontalSpacing: Spacing.md,
                            verticalSpacing: Spacing.sm
                        ) {
                            DetailRow(label: "Disk Usage", value: Format.size(entry.displaySize))
                            if entry.logicalSize != entry.displaySize {
                                // Secondary, labeled: never the headline.
                                DetailRow(label: "Logical Size", value: Format.size(entry.logicalSize))
                            }
                            if let total = model.selectedScan?.totalDiskSize, total > 0 {
                                // % of scan lives HERE, not in the tree's bar
                                // column — the bars are % of parent only.
                                DetailRow(
                                    label: "% of Scan",
                                    value: String(format: "%.1f%%",
                                                  Double(entry.displaySize) / Double(total) * 100)
                                )
                            }
                            if entry.isDir, let files = entry.fileCount, let dirs = entry.dirCount {
                                // Full-walk descendant counts from the wire —
                                // never counted client-side.
                                DetailRow(
                                    label: "Contents",
                                    value: "\(files.formatted()) files, \(dirs.formatted()) dirs"
                                )
                            }
                            if let modified = entry.modifiedAt {
                                DetailRow(
                                    label: "Modified",
                                    value: modified.formatted(date: .abbreviated, time: .shortened)
                                )
                            }
                            DetailRow(label: "Path", value: entry.path)
                            if let fileType = entry.fileType {
                                DetailRow(label: "Extension", value: ".\(fileType)")
                            }
                            if let category = entry.category {
                                DetailRow(label: "Category", value: category)
                            }
                            if entry.nlink > 1 && !entry.isDir {
                                // Hardlinked: deleting one name frees nothing
                                // until every link is gone.
                                DetailRow(label: "Hard Links", value: "\(entry.nlink)")
                            }
                        }
                        .padding(Spacing.md)
                    }
                }
            } else {
                emptyState
            }
        }
        .frame(minWidth: Size.inspectorMinWidth, maxWidth: Size.inspectorMaxWidth)
    }

    private var emptyState: some View {
        VStack(spacing: Spacing.md) {
            Image(systemName: "info.circle")
                .font(.system(size: Size.iconMedium))
                .foregroundStyle(Palette.textSecondary)
                .accessibilityHidden(true)
            Text("Select an \(Vocabulary.fileEntry.lowercased()) to inspect")
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(Spacing.md)
    }

    private func kindDescription(for entry: ScanEntry) -> String {
        guard let ext = entry.fileType else { return "File" }
        switch ext {
        case "rs": return "Rust Source"
        case "swift": return "Swift Source"
        case "py": return "Python Source"
        case "js": return "JavaScript"
        case "ts": return "TypeScript"
        case "json": return "JSON Document"
        case "toml": return "TOML Config"
        case "yaml", "yml": return "YAML Document"
        case "md": return "Markdown Document"
        case "txt": return "Plain Text"
        case "html": return "HTML Document"
        case "css": return "CSS Stylesheet"
        case "png": return "PNG Image"
        case "jpg", "jpeg": return "JPEG Image"
        case "gif": return "GIF Image"
        case "svg": return "SVG Image"
        case "pdf": return "PDF Document"
        case "zip": return "ZIP Archive"
        case "gz", "tar": return "Compressed Archive"
        case "dmg": return "Disk Image"
        case "lock": return "Lock File"
        default: return "\(ext.uppercased()) File"
        }
    }
}

struct DetailRow: View {
    let label: String
    let value: String

    var body: some View {
        GridRow {
            Text(label)
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
                .gridColumnAlignment(.trailing)
            Text(value)
                .font(Typography.caption)
                .textSelection(.enabled)
                .gridColumnAlignment(.leading)
        }
    }
}

struct EntryIcon: View {
    @Environment(ScansModel.self) private var model
    let entry: ScanEntry

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: Radius.card)
                .fill(iconColor.opacity(0.15))
            Image(systemName: iconName)
                .font(Typography.title)
                .foregroundStyle(iconColor)
        }
        .accessibilityHidden(true) // decorative; the header text carries it
    }

    private var iconName: String {
        if entry.isDir { return "folder.fill" }
        guard let ext = entry.fileType else { return "doc" }
        switch ext {
        case "rs", "swift", "py", "js", "ts", "go", "c", "cpp", "java", "rb":
            return "chevron.left.forwardslash.chevron.right"
        case "png", "jpg", "jpeg", "gif", "svg", "tiff", "bmp":
            return "photo"
        case "mp3", "wav", "aac", "flac":
            return "music.note"
        case "mp4", "mov", "mkv", "avi":
            return "film"
        case "pdf":
            return "doc.richtext"
        case "zip", "tar", "gz", "dmg":
            return "archivebox"
        case "json", "xml", "yaml", "yml", "toml":
            return "doc.badge.gearshape"
        case "md", "txt":
            return "doc.text"
        default:
            return "doc"
        }
    }

    private var iconColor: Color {
        entry.isDir
            ? Palette.accent
            : Palette.legendColor(slot: model.legendSlot(for: entry.fileType))
    }
}
