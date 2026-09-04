// The Largest Files tab: the paged flat table (global top-N by disk size,
// filename search — the things a tree can't do). Type filtering moved to
// the treemap's legend chips, which write the same model.fileTypeFilter;
// this view kept only the search field so ONE control owns the type axis
// and the two can never disagree. Paging is scroll-triggered: the last
// loaded row scrolling into view fetches the next page; the summary names
// the partiality ("first N of M") instead of letting a page read as a
// total. Disk usage is the headline column; logical stays in the inspector.

import SwiftUI
import DesignKit
import PhantomCore

struct FileListView: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        @Bindable var model = model

        VStack(spacing: 0) {
            HStack(spacing: Spacing.md) {
                TextField("Search filenames…", text: $model.fileSearch)
                    .textFieldStyle(.roundedBorder)
                    .frame(maxWidth: Size.searchMaxWidth)
                    .onSubmit {
                        Task { await model.loadFiles() }
                    }

                if let filter = model.fileTypeFilter {
                    // The legend chip owns type filtering; this just makes
                    // the active filter visible (and clearable) from here.
                    Button {
                        Task { await model.toggleLegendChip(filter) }
                    } label: {
                        Label(".\(filter) ✕", systemImage: "line.3.horizontal.decrease.circle")
                            .font(Typography.caption)
                    }
                    .buttonStyle(.borderless)
                    .help("Filtered by the legend; click to clear")
                }

                Spacer()

                if model.filesLoading {
                    ProgressView()
                        .controlSize(.small)
                }

                Text(summary)
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
            }
            .padding(.horizontal, Spacing.md)
            .padding(.vertical, Spacing.xs)

            Rectangle()
                .fill(Palette.separator)
                .frame(height: 1)
                .accessibilityHidden(true)

            Table(of: ScanEntry.self, selection: selectionBinding) {
                TableColumn("Name") { entry in
                    HStack(spacing: Spacing.xs) {
                        Image(systemName: "doc")
                            .foregroundStyle(
                                Palette.legendColor(slot: model.legendSlot(for: entry.fileType))
                            )
                            .font(Typography.caption)
                            .accessibilityHidden(true)
                        Text(entry.name)
                            .lineLimit(1)
                    }
                    // Scroll-triggered paging: table cells materialize
                    // lazily, so the LAST row's appearance means the user
                    // scrolled to the bottom of what's loaded.
                    .onAppear {
                        let path = entry.path
                        Task { await model.fileRowAppeared(path: path) }
                    }
                }
                .width(min: Size.tableNameColumnMin, ideal: Size.tableNameColumnIdeal)

                TableColumn("Disk Usage") { entry in
                    // displaySize == diskSize, THE headline number.
                    Text(Format.size(entry.displaySize))
                        .monospacedDigit()
                }
                .width(min: Size.tableSizeColumnMin, ideal: Size.tableSizeColumnIdeal)

                TableColumn("Type") { entry in
                    Text(entry.fileType ?? "—")
                        .foregroundStyle(Palette.textSecondary)
                }
                .width(min: Size.tableTypeColumnMin, ideal: Size.tableTypeColumnIdeal)

                TableColumn("Path") { entry in
                    Text(relativePath(for: entry))
                        .font(Typography.caption)
                        .foregroundStyle(Palette.textSecondary)
                        .lineLimit(1)
                        .help(entry.path)
                }
                .width(min: Size.tablePathColumnMin, ideal: Size.tablePathColumnIdeal)
            } rows: {
                ForEach(model.files) { entry in
                    TableRow(entry)
                }
            }
            .contextMenu(forSelectionType: ScanEntry.ID.self) { paths in
                if let path = paths.first {
                    Button("Reveal in Finder") { FinderActions.reveal(path) }
                    Button("Copy Path") { FinderActions.copyPath(path) }
                }
            }
            .onKeyPress(.space) {
                // Quick Look the selected file (folders preview too).
                guard let path = model.selectedEntryPath else { return .ignored }
                QuickLookController.shared.toggle(path: path)
                return .handled
            }
        }
    }

    /// Table selection rides the model's entry-path selection so the
    /// treemap highlight and inspector stay in sync with the table.
    private var selectionBinding: Binding<ScanEntry.ID?> {
        Binding(
            get: { model.selectedEntryPath },
            set: { newPath in Task { await model.selectEntry(path: newPath) } }
        )
    }

    /// Never lets a partial page read as a total: an unfiltered partial
    /// says "first N of M" (M = the scan's full-walk file count); filtered
    /// partials keep the "+" marker because no filtered total exists.
    private var summary: String {
        let loadedSize = model.files.reduce(UInt64(0)) { $0 + $1.displaySize }
        let count = model.files.count
        if model.filesNextCursor == nil {
            return "\(count) files — \(Format.size(loadedSize))"
        }
        if let total = model.filesTotalCount {
            return "first \(count) of \(total) files — \(Format.size(loadedSize)) loaded"
        }
        return "first \(count)+ matching files — \(Format.size(loadedSize)) loaded"
    }

    private func relativePath(for entry: ScanEntry) -> String {
        guard let root = model.selectedScan?.rootPath, entry.path.hasPrefix(root) else {
            return entry.path
        }
        let relative = String(entry.path.dropFirst(root.count))
        return relative.hasPrefix("/") ? String(relative.dropFirst()) : relative
    }
}
