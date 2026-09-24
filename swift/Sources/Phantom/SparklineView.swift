// A tiny line through a series of sizes, normalised to its own min/max —
// the sidebar's "is this folder growing" glance (v1.1 Phase 4). Pure
// drawing: the values come from ScansModel.sparklineValues, already
// oldest-first, already complete-only.

import SwiftUI
import DesignKit

struct Sparkline: View {
    let values: [UInt64]

    var body: some View {
        GeometryReader { geo in
            Path { path in
                guard values.count >= 2 else { return }
                let lo = Double(values.min() ?? 0)
                let hi = Double(values.max() ?? 0)
                let span = max(hi - lo, 1)
                let stepX = geo.size.width / Double(values.count - 1)
                for (i, v) in values.enumerated() {
                    let x = Double(i) * stepX
                    // Bigger is higher; a flat series draws a mid-height line.
                    let y = hi == lo ? geo.size.height / 2 : geo.size.height * (1 - (Double(v) - lo) / span)
                    if i == 0 { path.move(to: CGPoint(x: x, y: y)) } else { path.addLine(to: CGPoint(x: x, y: y)) }
                }
            }
            .stroke(trendColor, lineWidth: Size.sparklineLineWidth)
        }
        .accessibilityHidden(true) // the row composes its own label
    }

    /// Growing reads as a warning, shrinking as success, flat as neutral —
    /// the same vocabulary the forecast text uses.
    private var trendColor: Color {
        guard let first = values.first, let last = values.last else { return Palette.textSecondary }
        if last > first { return Palette.warning }
        if last < first { return Palette.success }
        return Palette.textSecondary
    }
}
