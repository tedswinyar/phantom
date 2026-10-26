import XCTest
@testable import DesignKit

final class TokenTests: XCTestCase {
    // Invariant: spacing is a strictly increasing 4pt-grid scale. A token
    // edit that breaks monotonicity breaks every layout built on it.
    func testSpacingScaleIsStrictlyIncreasingOnTheGrid() {
        let scale = [Spacing.xs, Spacing.sm, Spacing.md, Spacing.lg, Spacing.xl]
        for (a, b) in zip(scale, scale.dropFirst()) {
            XCTAssertLessThan(a, b)
        }
        for step in scale {
            XCTAssertEqual(step.truncatingRemainder(dividingBy: 4), 0,
                           "spacing tokens stay on the 4pt grid")
        }
    }

    /// The window floor is the sum of the panes it must hold with the
    /// inspector open and the sidebar dragged to its max — the AppKit-backed
    /// containers report no minimum upward, so this arithmetic is the ONLY
    /// thing that keeps the detail inside the window (phantom-xb1). Edit a
    /// pane band and the floor moves with it; a literal here would not.
    func testWindowFloorHoldsEveryPaneAtItsWidest() {
        XCTAssertGreaterThanOrEqual(
            Size.windowMinWidth,
            Size.sidebarMaxWidth + Size.detailMinWidth + Size.resizeHandleWidth + Size.inspectorMaxWidth)
        XCTAssertGreaterThanOrEqual(Size.resizeHandleWidth, Size.dividerWidth, "the handle holds the divider line")
        XCTAssertGreaterThanOrEqual(Size.inspectorDefaultWidth, Size.inspectorMinWidth)
        XCTAssertLessThanOrEqual(Size.inspectorDefaultWidth, Size.inspectorMaxWidth)
        XCTAssertLessThanOrEqual(Size.sidebarMinWidth, Size.sidebarIdealWidth)
        XCTAssertLessThanOrEqual(Size.sidebarIdealWidth, Size.sidebarMaxWidth)
        XCTAssertLessThanOrEqual(Size.inspectorMinWidth, Size.inspectorMaxWidth)
        // The detail's own floor exceeds what its tab row needs at rest
        // (448pt measured): the layout gate measures the row, this pins the
        // relationship between the floors.
        XCTAssertGreaterThan(Size.detailMinWidth, Size.treemapMinHeight,
            "a detail narrower than it is tall at the floor is a sign the tokens were edited blind")
    }

    func testRadiiAreSaneForControls() {
        XCTAssertGreaterThan(Radius.card, Radius.control,
                             "cards read as larger surfaces than controls")
    }
}
