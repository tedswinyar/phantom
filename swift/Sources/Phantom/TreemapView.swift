// The treemap (Specter Map). The LAYOUT is computed server-side at the
// view's ACTUAL size: the first fetch and every drill/crumb navigation send
// the real GeometryReader size, never a fixed canvas. A window RESIZE does
// not refetch — the fetched layout is rescaled client-side (the server's
// squarified aspect ratios drift slightly until the next navigation, which
// lays out fresh at the new size via the viewport seam).
//
// Interaction: tap a directory → drill in (root= refetch through the model,
// so the drill stack only moves when the fetch lands); tap a file → select
// it for the inspector; hover → tooltip with the headline (disk) size.

import SwiftUI
import DesignKit
import PhantomCore

struct TreemapView: View {
    @Environment(ScansModel.self) private var model
    // Hover tracks rect IDs, not paths: a residual pseudo-tile shares its
    // parent's PATH by contract (clicks resolve to the dir), so path-keyed
    // hover would light the parent and show the wrong tooltip.
    @State private var hoveredID: String?
    @State private var tooltipPosition: CGPoint = .zero

    var body: some View {
        VStack(spacing: 0) {
            map
            LegendStrip()
        }
    }

    private var map: some View {
        GeometryReader { geometry in
            ZStack(alignment: .topLeading) {
                if let layout = model.treemap {
                    Canvas { context, size in
                        draw(layout: layout, in: context, size: size)
                    }
                    .gesture(
                        SpatialTapGesture()
                            .onEnded { value in
                                handleTap(at: value.location, in: geometry.size)
                            }
                    )
                    .onContinuousHover { phase in
                        switch phase {
                        case .active(let location):
                            hoveredID = rect(at: location, in: geometry.size)?.id
                            tooltipPosition = location
                        case .ended:
                            hoveredID = nil
                        }
                    }
                    .accessibilityLabel("\(Vocabulary.treemapView) of \(layout.rootPath)")
                    .contextMenu {
                        // The continuous-hover position IS the right-click
                        // target: hover updates land before the menu opens.
                        // A residual's path is its parent dir — Reveal/Copy
                        // on the folder is the right answer there.
                        if let hovered = hoveredID,
                           let rect = model.treemap?.rects.first(where: { $0.id == hovered }) {
                            Button("Reveal in Finder") { FinderActions.reveal(rect.path) }
                            Button("Copy Path") { FinderActions.copyPath(rect.path) }
                        }
                    }

                    if let hovered = hoveredID,
                       let rect = model.treemap?.rects.first(where: { $0.id == hovered }) {
                        TreemapTooltip(rect: rect, rootPath: layout.rootPath)
                            .position(
                                x: min(tooltipPosition.x + TreemapStyle.tooltipOffsetX,
                                       geometry.size.width - TreemapStyle.tooltipOffsetX),
                                y: max(tooltipPosition.y - TreemapStyle.tooltipOffsetY,
                                       TreemapStyle.tooltipMinY)
                            )
                            .allowsHitTesting(false)
                    }
                } else {
                    emptyState
                }
            }
            // Fetch the layout at the view's REAL size when a terminal scan
            // is selected and no layout is loaded yet.
            .task(id: model.selectedScanID) {
                guard let scan = model.selectedScan, scan.isTerminal,
                      model.treemap == nil,
                      geometry.size.width > 0, geometry.size.height > 0 else { return }
                await model.loadTreemap(
                    scanID: scan.id,
                    width: geometry.size.width,
                    height: geometry.size.height,
                    maxDepth: TreemapStyle.requestDepth
                )
            }
            // Resize: remember the new viewport for the NEXT fetch; the
            // Canvas above already rescales the current layout for free.
            .onChange(of: geometry.size) { _, newSize in
                guard newSize.width > 0, newSize.height > 0 else { return }
                model.updateTreemapViewport(
                    width: newSize.width, height: newSize.height
                )
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: Spacing.md) {
            Image(systemName: "rectangle.split.3x3")
                .font(.system(size: Size.iconHero))
                .foregroundStyle(Palette.textSecondary)
                .accessibilityHidden(true)
            Text("The \(Vocabulary.treemap.lowercased()) appears once the \(Vocabulary.scan.lowercased()) finishes.")
                .font(Typography.body)
                .foregroundStyle(Palette.textSecondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(Spacing.md)
    }

    // MARK: - Drawing

    /// Scale factors from the layout's coordinate space (the size it was
    /// fetched at — the root rect spans it) to the current canvas size.
    private func scale(of layout: TreemapLayout, to size: CGSize) -> (x: CGFloat, y: CGFloat)? {
        guard let root = layout.rects.first, root.width > 0, root.height > 0 else { return nil }
        return (size.width / root.width, size.height / root.height)
    }

    private func draw(layout: TreemapLayout, in context: GraphicsContext, size: CGSize) {
        guard let scale = scale(of: layout, to: size) else { return }

        // Shallowest first so children draw over their parents. Children
        // COVER their parent exactly (the server subdivides the full rect),
        // so a visible rect set determines what the eye actually sees.
        let ordered = layout.rects.sorted { $0.depth < $1.depth }
        var visible: [(rect: TreemapRect, scaled: CGRect)] = []
        visible.reserveCapacity(ordered.count)
        for rect in ordered {
            let scaled = CGRect(
                x: rect.x * scale.x, y: rect.y * scale.y,
                width: rect.width * scale.x, height: rect.height * scale.y
            )
            if scaled.width > TreemapStyle.minRectDimension,
               scaled.height > TreemapStyle.minRectDimension {
                visible.append((rect, scaled))
            }
        }

        // No painted text: every always-on labeling scheme tried here
        // (centered, corner chips, collision-placed) still read as clutter
        // on real trees — nested dirs share corners and midpoints. Names
        // live on the hover tooltip (with the relative path) and in the
        // breadcrumb after a drill.
        for (rect, scaled) in visible {
            let isHovered = hoveredID == rect.id
            // A residual shares its parent's path; without the guard,
            // selecting the folder would stroke the pseudo-tile too.
            let isSelected = model.selectedEntryPath == rect.path && !rect.residual

            if rect.residual {
                // The "smaller files" pseudo-tile: neutral, clearly not a
                // typed file, clearly not empty space. Never a legend match
                // (it has no type), so any live chip selection dims it.
                var opacity = isHovered
                    ? TreemapStyle.residualFillHoverOpacity : TreemapStyle.residualFillOpacity
                if model.legendSelection != .none {
                    opacity *= TreemapStyle.legendDimFactor
                }
                context.fill(Path(scaled), with: .color(Palette.textPrimary.opacity(opacity)))
            } else if rect.isDir {
                // The faint resting fill remains for dirs whose shortfall is
                // below the server's residual threshold — tiny gaps must
                // still read as occupied, and the fill doubles as a depth
                // cue in deep chains.
                let opacity = isSelected
                    ? TreemapStyle.dirFillSelectedOpacity
                    : isHovered ? TreemapStyle.dirFillHoverOpacity : TreemapStyle.dirFillOpacity
                context.fill(Path(scaled), with: .color(Palette.textPrimary.opacity(opacity)))
            } else {
                var opacity = isSelected
                    ? TreemapStyle.fileFillSelectedOpacity
                    : isHovered ? TreemapStyle.fileFillHoverOpacity : TreemapStyle.fileFillOpacity
                // A live legend-chip selection dims every non-matching tile.
                if case .type(let highlighted) = model.legendSelection,
                   rect.fileType != highlighted {
                    opacity *= TreemapStyle.legendDimFactor
                }
                let color = Palette.legendColor(slot: model.legendSlot(for: rect.fileType))
                context.fill(Path(scaled), with: .color(color.opacity(opacity)))
            }

            if isSelected {
                context.stroke(
                    Path(scaled), with: .color(Palette.accent),
                    lineWidth: TreemapStyle.selectionLineWidth
                )
            }
            let borderOpacity = rect.isDir
                ? TreemapStyle.dirBorderOpacity : TreemapStyle.fileBorderOpacity
            context.stroke(
                Path(scaled),
                with: .color(Palette.textPrimary.opacity(borderOpacity)),
                lineWidth: rect.isDir ? TreemapStyle.dirLineWidth : TreemapStyle.fileLineWidth
            )

        }
    }

    // MARK: - Hit testing

    /// The deepest rect under the point — files win over their containing
    /// directories; among directories the deepest (smallest) wins, so a tap
    /// in a directory's padding drills into THAT directory, not the root.
    private func rect(at point: CGPoint, in size: CGSize) -> TreemapRect? {
        guard let layout = model.treemap, let scale = scale(of: layout, to: size) else { return nil }
        let hits = layout.rects.filter { rect in
            CGRect(
                x: rect.x * scale.x, y: rect.y * scale.y,
                width: rect.width * scale.x, height: rect.height * scale.y
            ).contains(point)
        }
        return hits.first { !$0.isDir } ?? hits.max { $0.depth < $1.depth }
    }

    private func handleTap(at point: CGPoint, in size: CGSize) {
        guard let rect = rect(at: point, in: size) else { return }
        // Every tap also REVEALS the tile in the Folders tree (expanding
        // ancestors, fetching uncached levels) — the map and the tree are
        // two views of one selection.
        if rect.isDir {
            // The depth-0 rect IS the current root; re-drilling it would
            // push a duplicate crumb. Inspect it instead.
            if rect.depth == 0 {
                Task {
                    await model.selectEntry(path: rect.path)
                    await model.revealInTree(path: rect.path)
                }
            } else {
                Task {
                    await model.drillIn(to: rect.path)
                    await model.revealInTree(path: rect.path)
                }
            }
        } else {
            Task {
                await model.selectEntry(path: rect.path)
                await model.revealInTree(path: rect.path)
            }
        }
    }
}

struct TreemapTooltip: View {
    let rect: TreemapRect
    let rootPath: String

    /// Where this rect sits under the current treemap root — with no
    /// painted labels on the map, the tooltip carries orientation.
    private var relativePath: String {
        let prefix = rootPath.hasSuffix("/") ? rootPath : rootPath + "/"
        guard rect.path.hasPrefix(prefix) else { return rect.path }
        return String(rect.path.dropFirst(prefix.count))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Spacing.xs) {
            Text(rect.name)
                .font(Typography.caption.bold())
            if relativePath != rect.name {
                Text(relativePath)
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: Size.tooltipMaxWidth, alignment: .leading)
            }
            // `size` is the aggregated DISK size — the headline number.
            Text(Format.size(rect.size))
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
            if rect.residual {
                Text("aggregated small files in this folder")
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
            } else if let fileType = rect.fileType {
                Text(".\(fileType)")
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
            }
        }
        .padding(Spacing.sm)
        .background(.ultraThickMaterial)
        .clipShape(RoundedRectangle(cornerRadius: Radius.control))
        .shadow(radius: Radius.control)
    }
}

/// The legend: a compact interactive chip strip on the treemap's bottom
/// edge — swatch + .ext + size for the scan's top-nine types plus grey
/// "other". Chip click highlights matching tiles (dimming the rest) AND
/// filters Largest Files; the same chip again clears both. Colors are
/// assigned per scan (biggest type takes slot 0) — the strip being always
/// visible is what makes per-scan colors okay.
struct LegendStrip: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        if !model.typeTotals.isEmpty {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: Spacing.sm) {
                    ForEach(Array(model.typeTotals.prefix(9).enumerated()), id: \.offset) { slot, total in
                        chip(
                            label: total.fileType.map { ".\($0)" } ?? "no ext",
                            size: total.diskSize,
                            fileCount: total.fileCount,
                            color: Palette.legendColor(slot: slot),
                            isOn: model.legendSelection == .type(total.fileType)
                        ) {
                            Task { await model.toggleLegendChip(total.fileType) }
                        }
                    }
                    if model.typeTotals.count > 9 {
                        let rest = model.typeTotals.dropFirst(9)
                        chip(
                            label: "other",
                            size: rest.reduce(0) { $0 + $1.diskSize },
                            fileCount: rest.reduce(0) { $0 + $1.fileCount },
                            color: Palette.legendOther,
                            isOn: false,
                            action: nil
                        )
                    }
                }
                .padding(.horizontal, Spacing.md)
                .padding(.vertical, Spacing.xs)
            }
            .background(Palette.cardBackground)
        }
    }

    private func chip(
        label: String, size: UInt64, fileCount: UInt64, color: Color,
        isOn: Bool, action: (() -> Void)?
    ) -> some View {
        let content = HStack(spacing: Spacing.xs) {
            Circle()
                .fill(color)
                .frame(width: TreeStyle.swatchSize, height: TreeStyle.swatchSize)
                .accessibilityHidden(true)
            Text(label)
                .font(Typography.caption)
            Text(Format.size(size))
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
                .monospacedDigit()
        }
        .padding(.horizontal, Spacing.sm)
        .padding(.vertical, Spacing.xs)
        .background(isOn ? color.opacity(0.2) : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: Radius.control))

        return Group {
            if let action {
                Button(action: action) { content }
                    .buttonStyle(.plain)
                    .help("\(fileCount) files — click to highlight on the map and filter Largest Files")
                    .accessibilityLabel("\(label), \(Format.size(size))\(isOn ? ", highlighted" : "")")
            } else {
                content
                    .help("\(fileCount) files — everything beyond the top nine types")
                    .accessibilityLabel("\(label), \(Format.size(size))")
            }
        }
    }
}
