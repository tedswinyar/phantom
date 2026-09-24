// The treemap drill trail. Crumbs come straight from ScansModel.breadcrumbs
// (scan root + drill stack); tapping one jumps the treemap to that level via
// a root= refetch. Hidden while the trail is only the scan root.

import SwiftUI
import DesignKit
import PhantomCore

struct BreadcrumbBar: View {
    @Environment(ScansModel.self) private var model

    var body: some View {
        let crumbs = model.breadcrumbs
        if crumbs.count > 1 {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: Spacing.xs) {
                    ForEach(Array(crumbs.enumerated()), id: \.offset) { index, path in
                        if index > 0 {
                            Image(systemName: "chevron.right")
                                .font(Typography.caption)
                                .foregroundStyle(Palette.textSecondary)
                                .accessibilityHidden(true)
                        }
                        Button {
                            Task { await model.navigate(toCrumb: index) }
                        } label: {
                            Text(displayName(for: path))
                                .font(Typography.caption)
                                .foregroundStyle(
                                    index == crumbs.count - 1
                                        ? Palette.textPrimary : Palette.textSecondary
                                )
                        }
                        .buttonStyle(.plain)
                        .disabled(index == crumbs.count - 1)
                        .accessibilityLabel("Go to \(displayName(for: path))")
                    }
                }
                .padding(.horizontal, Spacing.md)
                .padding(.vertical, Spacing.xs)
            }
            .background(Palette.cardBackground)
            Rectangle()
                .fill(Palette.separator)
                .frame(height: 1)
                .accessibilityHidden(true)
        }
    }

    private func displayName(for path: String) -> String {
        (path as NSString).lastPathComponent
    }
}
