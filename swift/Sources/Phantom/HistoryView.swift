// The History pane (v1.1 Phase 4, phantom-mkn.11): how the selected root
// grew across its completed scans, the breakdown by the chosen groupBy,
// and the linear forecast — ALWAYS with its caveat. Nothing here computes
// a trend: the server's Growth object is rendered as-is, and the chart is
// the points' totals joined by a line.
//
// Phantom never deletes; this pane has no affordance beyond the picker.

import SwiftUI
import DesignKit
import PhantomCore

struct HistoryView: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        @Bindable var model = model
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: Spacing.md) {
                Picker("Break down by", selection: $model.growthGroupBy) {
                    ForEach(GrowthGroupBy.allCases, id: \.self) { g in
                        Text(g.label).tag(g)
                    }
                }
                .pickerStyle(.menu)
                .fixedSize()
                .onChange(of: model.growthGroupBy) { _, _ in
                    Task { await model.loadGrowth() }
                }
                Spacer()
                if let g = model.growth {
                    Text("\(g.points.count) \(Vocabulary.scan.lowercased())\(g.points.count == 1 ? "" : "s") of \(g.rootPath)")
                        .font(Typography.caption)
                        .foregroundStyle(Palette.textSecondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            .padding(.horizontal, Spacing.md)
            .padding(.vertical, Spacing.xs)

            if let g = model.growth {
                ScrollView {
                    VStack(alignment: .leading, spacing: Spacing.md) {
                        chart(g)
                        forecast(g)
                        seriesTable(g)
                        compare
                        Text(g.note)
                            .font(Typography.caption)
                            .foregroundStyle(Palette.textSecondary)
                    }
                    .padding(Spacing.md)
                }
            } else if model.growthUnavailable {
                message("No completed \(Vocabulary.scan.lowercased()) of this folder yet — history starts with the first one.")
            } else {
                message("Loading history…")
            }
        }
    }

    // MARK: - Compare any two (Phase 4, phantom-mkn.23.1)

    /// Pick another complete scan of this root; the deltas always read
    /// older → newer whichever the user chose. Proves a plan's estimate
    /// before/after without leaving the app.
    @ViewBuilder private var compare: some View {
        @Bindable var model = model
        let others = model.comparableScans
        if !others.isEmpty {
            VStack(alignment: .leading, spacing: Spacing.xs) {
                HStack(spacing: Spacing.sm) {
                    Text("Compare with")
                        .font(Typography.rowTitle)
                    Picker("Compare with", selection: $model.compareWithID) {
                        Text("—").tag(UUID?.none)
                        ForEach(others) { scan in
                            Text("\(scan.startedAt.formatted(date: .abbreviated, time: .shortened))  \(Format.size(scan.totalDiskSize))")
                                .tag(Optional(scan.id))
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    .onChange(of: model.compareWithID) { _, _ in
                        Task { await model.loadComparison() }
                    }
                }
                if let d = model.comparison {
                    comparisonSummary(d)
                }
            }
        }
    }

    private func comparisonSummary(_ d: ScanDiff) -> some View {
        VStack(alignment: .leading, spacing: Spacing.xs) {
            Text("\(d.scanAStartedAt.formatted(date: .abbreviated, time: .shortened)) → \(d.scanBStartedAt.formatted(date: .abbreviated, time: .shortened)):  \(signed(d.diskDelta)) on disk, \(signedCount(d.fileCountDelta)) files")
                .font(Typography.rowTitle)
            if !d.grown.isEmpty {
                Text("Grew").font(Typography.caption).foregroundStyle(Palette.textSecondary)
                ForEach(d.grown.prefix(8), id: \.path) { e in
                    movementRow(e, color: Palette.warning)
                }
            }
            if !d.freed.isEmpty {
                Text("Freed").font(Typography.caption).foregroundStyle(Palette.textSecondary)
                ForEach(d.freed.prefix(8), id: \.path) { e in
                    movementRow(e, color: Palette.success)
                }
            }
            if d.grown.isEmpty && d.freed.isEmpty {
                Text("No directory moved by a megabyte or more.")
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
            }
        }
        .padding(Spacing.sm)
        .background(Palette.cardBackground, in: RoundedRectangle(cornerRadius: Radius.card))
    }

    private func movementRow(_ e: DiffEntry, color: Color) -> some View {
        HStack(spacing: Spacing.sm) {
            Text(e.path)
                .font(Typography.caption)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer()
            Text(e.before == nil ? "new  \(signed(e.delta))" : (e.after == nil ? "gone  \(signed(e.delta))" : signed(e.delta)))
                .font(Typography.caption)
                .foregroundStyle(color)
                .monospacedDigit()
        }
        .accessibilityElement(children: .combine)
    }

    /// Same sign glyphs as `signed` so the header reads "−126 GB, −304,552".
    private func signedCount(_ n: Int64) -> String {
        n >= 0 ? "+\(n.formatted())" : "−\(n.magnitude.formatted())"
    }

    private func signed(_ bytes: Int64) -> String {
        bytes >= 0 ? "+\(Format.size(UInt64(bytes)))" : "−\(Format.size(UInt64(bytes.magnitude)))"
    }

    // MARK: - Pieces

    private func message(_ text: String) -> some View {
        Text(text)
            .font(Typography.caption)
            .foregroundStyle(Palette.textSecondary)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding(Spacing.md)
    }

    /// The totals line with its first/last labels. One point is a dot's
    /// worth of information, so it renders as text instead.
    @ViewBuilder private func chart(_ g: Growth) -> some View {
        if g.points.count >= 2 {
            VStack(alignment: .leading, spacing: Spacing.xs) {
                Sparkline(values: g.totals)
                    .frame(height: Size.chartHeight)
                    .frame(maxWidth: .infinity)
                    .background(Palette.cardBackground, in: RoundedRectangle(cornerRadius: Radius.card))
                HStack {
                    if let first = g.points.first {
                        label(first)
                    }
                    Spacer()
                    if let last = g.points.last {
                        label(last)
                    }
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("History chart: \(Format.size(g.totals.first ?? 0)) to \(Format.size(g.totals.last ?? 0)) over \(g.points.count) scans")
        } else if let only = g.points.first {
            HStack(spacing: Spacing.sm) {
                Text("One \(Vocabulary.scan.lowercased()) so far:")
                label(only)
                Text("— \(Vocabulary.scan.lowercased()) again later for a trend.")
            }
            .font(Typography.caption)
            .foregroundStyle(Palette.textSecondary)
        }
    }

    private func label(_ p: GrowthPoint) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(Format.size(p.totalDiskSize))
                .font(Typography.rowTitle)
            Text(p.startedAt, format: .dateTime.year().month().day())
                .font(Typography.caption)
                .foregroundStyle(Palette.textSecondary)
        }
    }

    /// The number and its caveat are one element: the caveat is never
    /// hidden behind a disclosure.
    @ViewBuilder private func forecast(_ g: Growth) -> some View {
        if let f = g.forecast {
            VStack(alignment: .leading, spacing: Spacing.xs) {
                HStack(spacing: Spacing.sm) {
                    Image(systemName: f.isGrowing ? "arrow.up.right" : (f.bytesPerDay < 0 ? "arrow.down.right" : "arrow.right"))
                        .foregroundStyle(f.isGrowing ? Palette.warning : Palette.success)
                        .accessibilityHidden(true)
                    Text(forecastLine(f))
                        .font(Typography.rowTitle)
                }
                Text(f.caveat)
                    .font(Typography.caption)
                    .foregroundStyle(Palette.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(Spacing.sm)
            .background(Palette.cardBackground, in: RoundedRectangle(cornerRadius: Radius.card))
            .accessibilityElement(children: .combine)
        }
    }

    private func forecastLine(_ f: GrowthForecast) -> String {
        let rate = f.bytesPerDay >= 0
            ? "+\(Format.size(UInt64(f.bytesPerDay)))/day"
            : "−\(Format.size(UInt64(f.bytesPerDay.magnitude)))/day"
        if let days = f.daysUntilFull, let at = f.projectedFullAt {
            return "\(rate) over \(String(format: "%.1f", f.spanDays)) days — disk full in \(Int(days.rounded())) days (\(at.formatted(.dateTime.year().month().day()))) at this rate"
        }
        if f.isGrowing {
            return "\(rate) over \(String(format: "%.1f", f.spanDays)) days — the volume's headroom is unknown"
        }
        return "\(rate) over \(String(format: "%.1f", f.spanDays)) days — not growing"
    }

    /// One row per series line: the key, first → last, and the change.
    @ViewBuilder private func seriesTable(_ g: Growth) -> some View {
        if !g.series.isEmpty {
            VStack(alignment: .leading, spacing: Spacing.xs) {
                ForEach(g.series) { line in
                    HStack(spacing: Spacing.sm) {
                        Text(line.key)
                            .font(Typography.rowTitle)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer()
                        Text(valuesSummary(line))
                            .font(Typography.caption)
                            .foregroundStyle(Palette.textSecondary)
                            .monospacedDigit()
                    }
                    .accessibilityElement(children: .combine)
                }
            }
        }
    }

    private func valuesSummary(_ line: GrowthLine) -> String {
        let first = line.values.first ?? nil
        let last = line.values.last ?? nil
        switch (first, last) {
        case let (a?, b?) where b >= a:
            return "\(Format.size(a)) → \(Format.size(b))  (+\(Format.size(b - a)))"
        case let (a?, b?):
            return "\(Format.size(a)) → \(Format.size(b))  (−\(Format.size(a - b)))"
        case (nil, let b?):
            return "— → \(Format.size(b))"
        default:
            return "—"
        }
    }
}
