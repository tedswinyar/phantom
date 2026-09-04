// DesignKit — the design tokens for Phantom. Views use ONLY these
// tokens; raw literals for spacing, radius, or color roles in view code
// are a review finding (skills/swift-conventions.md).

import SwiftUI

public enum Spacing {
    /// 4pt grid. Views compose these; they never hardcode point values.
    public static let xs: CGFloat = 4
    public static let sm: CGFloat = 8
    public static let md: CGFloat = 16
    public static let lg: CGFloat = 24
    public static let xl: CGFloat = 40
}

/// Element SIZES — distinct from Spacing. A dot's diameter is not a margin;
/// reusing a Spacing value as a size (the old `frame(width: Spacing.sm)`)
/// couples unrelated things — bump the spacing scale and the dot resizes
/// (design M6). Sizes get their own semantic tokens.
public enum Size {
    /// Diameter of the scan status dot.
    public static let statusDot: CGFloat = 8
    /// Widest a hover tooltip's path line may grow before middle-truncating.
    public static let tooltipMaxWidth: CGFloat = 320
    /// Reserved width of the trailing metadata (date) column, so the date
    /// sits in a stable column regardless of per-row affordances (design H2).
    public static let metaColumn: CGFloat = 96

    // Ghost app layout sizes (ported from v0.1's raw literals; tokens now).
    /// Empty-state / inspector-header glyph sizes, small → hero.
    public static let iconMedium: CGFloat = 36
    public static let iconLarge: CGFloat = 48
    public static let iconHero: CGFloat = 64
    /// Inspector panel width band.
    public static let inspectorMinWidth: CGFloat = 240
    public static let inspectorMaxWidth: CGFloat = 300
    /// Treemap pane never collapses below this.
    public static let treemapMinHeight: CGFloat = 200
    /// File list pane height band.
    public static let fileListMinHeight: CGFloat = 150
    public static let fileListIdealHeight: CGFloat = 250
    /// Sheet width band — flexible so large Dynamic Type reflows, not clips.
    public static let sheetMinWidth: CGFloat = 380
    public static let sheetIdealWidth: CGFloat = 420
    public static let sheetMaxWidth: CGFloat = 520
    /// File-list toolbar control widths.
    public static let filterMaxWidth: CGFloat = 300
    public static let searchMaxWidth: CGFloat = 200
    /// Window floor (floors, not fixed sizes — content can grow the window).
    public static let windowMinWidth: CGFloat = 720
    public static let windowMinHeight: CGFloat = 480
    /// File-table column bands (min/ideal), name → size → type → path.
    public static let tableNameColumnMin: CGFloat = 150
    public static let tableNameColumnIdeal: CGFloat = 250
    public static let tableSizeColumnMin: CGFloat = 70
    public static let tableSizeColumnIdeal: CGFloat = 90
    public static let tableTypeColumnMin: CGFloat = 50
    public static let tableTypeColumnIdeal: CGFloat = 70
    public static let tablePathColumnMin: CGFloat = 200
    public static let tablePathColumnIdeal: CGFloat = 400
}

/// Treemap drawing constants — the Canvas is procedural, so its "design
/// tokens" are numbers, not view modifiers. One namespace so a treemap
/// restyle never means hunting magic numbers through drawing code.
public enum TreemapStyle {
    /// Rects thinner than this (either axis, in points) are not drawn.
    public static let minRectDimension: CGFloat = 1
    /// Layout depth requested from the server. The API's own default (4)
    /// stops exactly at deep build dirs (target/debug/deps), leaving giant
    /// featureless slabs; deeper layout renders their file tiles. Persisted
    /// entries are dirs + files >=1 MiB, so the rect count stays bounded.
    public static let requestDepth: Int = 6

    public static let selectionLineWidth: CGFloat = 4
    public static let dirLineWidth: CGFloat = 1.5
    public static let fileLineWidth: CGFloat = 0.5

    /// Directory fill opacities: rest / hovered / selected.
    public static let dirFillOpacity: Double = 0.04
    public static let dirFillHoverOpacity: Double = 0.08
    public static let dirFillSelectedOpacity: Double = 0.15
    /// File fill opacities: rest / hovered / selected.
    public static let fileFillOpacity: Double = 0.7
    public static let fileFillHoverOpacity: Double = 0.9
    public static let fileFillSelectedOpacity: Double = 1.0
    /// Border opacities over Palette.textPrimary.
    public static let dirBorderOpacity: Double = 0.3
    public static let fileBorderOpacity: Double = 0.15

    /// Non-matching tiles fade to this fraction of their normal opacity
    /// while a legend chip is highlighting a type.
    public static let legendDimFactor: Double = 0.22

    /// The residual "smaller files" pseudo-tile: neutral fill, clearly
    /// heavier than a dir's resting fill (0.04) so it reads as a THING,
    /// clearly lighter than typed tiles so it never reads as one of them.
    public static let residualFillOpacity: Double = 0.12
    public static let residualFillHoverOpacity: Double = 0.18

    /// Tooltip placement: offset from the pointer, clamped inside the view.
    public static let tooltipOffsetX: CGFloat = 80
    public static let tooltipOffsetY: CGFloat = 30
    public static let tooltipMinY: CGFloat = 20
}

/// Folders-tree drawing constants — the % of Parent bar and type swatch are
/// drawn in AppKit cells; their tokens live here like TreemapStyle's.
public enum TreeStyle {
    /// The % of Parent bar's track.
    public static let barWidth: CGFloat = 72
    public static let barHeight: CGFloat = 6
    public static let barFillOpacity: Double = 0.55
    public static let barTrackOpacity: Double = 0.12
    /// The file-type swatch beside the name.
    public static let swatchSize: CGFloat = 8
    /// Reserved width of the percent label so bars align in a column.
    public static let percentLabelWidth: CGFloat = 40
}

public enum Radius {
    public static let card: CGFloat = 10
    public static let control: CGFloat = 6
}

/// Semantic color roles. Views name the ROLE, not the color, so a theme
/// swap is a one-file change. Every color used in a view must have a role
/// here — a raw `.red` in view code is a review finding (design M5).
public enum Palette {
    /// The brand accent. Applied at the app root via `.tint(Palette.accent)`
    /// so prominent controls use it (design H4 — it was dead code before).
    public static let accent = Color.purple
    public static let background = Color(nsColor: .windowBackgroundColor)
    public static let cardBackground = Color(nsColor: .controlBackgroundColor)
    public static let textPrimary = Color.primary
    public static let textSecondary = Color.secondary
    /// Status roles: reuse these for any semantic pass/fail signal so
    /// views never reach for a raw color.
    public static let success = Color.green
    public static let warning = Color.orange
    public static let error = Color.red
    /// Informational, non-actionable signal (cloud-placeholder badges).
    public static let info = Color.cyan
    public static let separator = Color(nsColor: .separatorColor)

    // MARK: Legend palette — the file-type axis.
    //
    // Nine fixed slots plus grey "other": categorical perception tops out
    // around 8–12 hues and thin treemap tiles are the hard case, so the
    // palette is CAPPED, not rotating (WinDirStat's unlimited palette is its
    // dated part). Slots are assigned PER SCAN by descending per-type disk
    // size — colors change between scans BY DESIGN, and that is fine because
    // the legend strip is always visible to anchor them.
    //
    // Hue design: nine vivid mid-lightness anchors spaced around the wheel,
    // tuned so no neighboring pair collapses at swatch size in either
    // appearance (the old role palette's cyan-vs-mint and brown-vs-orange
    // both failed this). The teal slot is pushed blue-green away from the
    // green slot, and azure/periwinkle/violet are separated by lightness as
    // well as hue — periwinkle-vs-violet remains the closest pair and gets
    // non-adjacent ranks only when a scan has 7+ types anyway. Fixed sRGB
    // values (not system colors): mid tones read on both light and dark.
    public static let legend: [Color] = [
        Color(red: 0.90, green: 0.28, blue: 0.30), // 0 red
        Color(red: 0.94, green: 0.53, blue: 0.24), // 1 orange
        Color(red: 0.92, green: 0.77, blue: 0.31), // 2 gold
        Color(red: 0.31, green: 0.75, blue: 0.25), // 3 green
        Color(red: 0.17, green: 0.78, blue: 0.72), // 4 teal (blue-green, away from green)
        Color(red: 0.23, green: 0.65, blue: 0.94), // 5 azure (light)
        Color(red: 0.48, green: 0.55, blue: 0.97), // 6 periwinkle
        Color(red: 0.60, green: 0.31, blue: 0.94), // 7 violet (deep)
        Color(red: 0.94, green: 0.37, blue: 0.72), // 8 magenta
    ]
    /// Everything beyond the top nine types (and typeless entries when the
    /// no-extension bucket isn't itself a top slot).
    public static let legendOther = Color(red: 0.56, green: 0.56, blue: 0.58)

    /// The color for a legend slot index (0..<9); nil or out-of-range means
    /// "other". Slot assignment itself lives in ScansModel (per-scan,
    /// testable); this is only the slot → hue lookup.
    public static func legendColor(slot: Int?) -> Color {
        guard let slot, legend.indices.contains(slot) else { return legendOther }
        return legend[slot]
    }

    /// Map a wire reclaimability `category` to its color role: safely
    /// reclaimable reads as success, review-only as a warning (never a
    /// suggestion), cloud placeholders as data-flavored information, and
    /// anything unknown falls back to the quiet secondary text role so a
    /// new server-side category degrades instead of shouting.
    public static func categoryColor(for category: String?) -> Color {
        switch category {
        case "staleProjectArtifact", "regenerableArtifact", "cache", "toolManagedCache":
            return success
        case "reviewFirst", "wontRegenerate":
            return warning
        case "cloudDataloaded":
            return info
        default:
            return textSecondary
        }
    }

}

public enum Typography {
    public static let title = Font.title2.weight(.semibold)
    /// A row/list title: heavier than body so titles read as titles, lighter
    /// than the section `title` (design M2 — body-weight titles didn't read).
    public static let rowTitle = Font.body.weight(.semibold)
    public static let body = Font.body
    public static let caption = Font.caption
    public static let mono = Font.system(.body, design: .monospaced)
}
