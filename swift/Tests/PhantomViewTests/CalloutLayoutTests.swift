// Layout gate for the unreadable-items callout (phantom-di7): the row stays
// one control tall at the detail column's floor width in both states, and
// the popover body at its fixed width holds a full 100-path sample inside
// its height cap. Headless NSHostingView, like WindowFloorLayoutTests.

import XCTest
import AppKit
import SwiftUI
@testable import Phantom
@testable import PhantomCore
import DesignKit

@MainActor
final class CalloutLayoutTests: XCTestCase {
    /// The callout spans the detail column; at the window floor with the
    /// sidebar at its max that is this wide.
    private var detailColumnWidth: CGFloat { Size.windowMinWidth - Size.sidebarMaxWidth }

    private func overflows(in view: NSView) -> [String] {
        if view is NSScrollView { return [] }
        var found: [String] = []
        for child in view.subviews {
            // AppKit's key-view-loop helper, parked off-bounds by design
            // wherever a popover anchor exists; invisible, not layout.
            if String(describing: type(of: child)) == "KeyViewProxy" { continue }
            let f = child.frame, b = view.bounds
            if f.width > 0, f.height > 0,
               f.minX < b.minX - 0.5 || f.minY < b.minY - 0.5 || f.maxX > b.maxX + 0.5 || f.maxY > b.maxY + 0.5 {
                found.append("\(type(of: child)) \(f) in \(type(of: view)) \(b)")
            }
            found += overflows(in: child)
        }
        return found
    }

    private func row(count: UInt64, fdaMissing: Bool) -> some View {
        ScanErrorsCalloutRow(count: count, fdaMissing: fdaMissing, showDetails: .constant(false),
                             onSettings: {}, onRescan: {}) { EmptyView() }
    }

    /// One control tall: the label truncates instead of wrapping, in the
    /// plain state and in the longer Full-Disk-Access state with its third
    /// button, at the floor width and at a generous one.
    func testCalloutRowStaysOneControlTall() {
        // The row's own single-line height: the short label, wide host.
        let oneLine = NSHostingView(rootView: row(count: 1, fdaMissing: false).frame(width: 1400)).fittingSize.height
        XCTAssertGreaterThan(oneLine, 16, "the row measured as nothing: \(oneLine)")
        for fdaMissing in [false, true] {
            for width in [detailColumnWidth, 1400] {
                let hosting = NSHostingView(rootView: row(count: 1_234_567, fdaMissing: fdaMissing).frame(width: width))
                let size = hosting.fittingSize
                XCTAssertLessThanOrEqual(size.height, oneLine + 2,
                    "fdaMissing=\(fdaMissing) at \(width)pt: row is \(size.height)pt, one line is \(oneLine)pt — the label wrapped")
                hosting.frame = CGRect(origin: .zero, size: size)
                hosting.layoutSubtreeIfNeeded()
                XCTAssertEqual(overflows(in: hosting), [], "fdaMissing=\(fdaMissing) at \(width)pt: a control left the row")
                let buttons = hosting.subviews.filter { !$0.frame.isEmpty }.count
                XCTAssertEqual(buttons, fdaMissing ? 3 : 2, "fdaMissing=\(fdaMissing): expected the Settings button only when the grant is missing")
            }
        }
    }

    /// The popover body with the biggest sample the wire carries (100
    /// paths, every cause) stays inside its height cap plus the header and
    /// footer, and nothing leaves its bounds.
    func testUnreadableItemsListHoldsAFullSampleInsideItsCap() {
        let reasons = [
            "Operation not permitted (os error 1)", "Interrupted system call (os error 4)",
            "Operation timed out (os error 60)", "Input/output error (os error 5)",
        ]
        let sample = (0..<100).map { i in
            UnreadablePath(path: "/Users/margaret/Library/Containers/com.example.app\(i)/Data/Library/Caches/very/deep/path/file-\(i).dat",
                           reason: reasons[i % reasons.count])
        }
        for fdaMissing in [false, true] {
            let list = UnreadableItemsList(errorCount: 2_500, sample: sample, fdaMissing: fdaMissing, onSettings: {}, onRescan: {})
                .frame(width: Size.popoverWidth)
            let hosting = NSHostingView(rootView: list)
            let size = hosting.fittingSize
            XCTAssertEqual(size.width, Size.popoverWidth)
            // header (title) + capped scroll body + sample note + button row + padding
            XCTAssertLessThanOrEqual(size.height, Size.popoverMaxHeight + 160,
                "fdaMissing=\(fdaMissing): popover body is \(size.height)pt — the list is not capped")
            XCTAssertGreaterThan(size.height, Size.popoverMaxHeight,
                "fdaMissing=\(fdaMissing): a 100-path sample should fill the cap; got \(size.height)")
            hosting.frame = CGRect(origin: .zero, size: size)
            hosting.layoutSubtreeIfNeeded()
            XCTAssertEqual(overflows(in: hosting), [], "fdaMissing=\(fdaMissing): content left the popover")
        }
    }

    /// The two honest empty states render (no sample recorded; recorded
    /// none) without the scroll body.
    func testUnreadableItemsListEmptyStatesAreShort() {
        for sample: [UnreadablePath]? in [nil, []] {
            let list = UnreadableItemsList(errorCount: 3, sample: sample, fdaMissing: false, onSettings: {}, onRescan: {})
                .frame(width: Size.popoverWidth)
            let size = NSHostingView(rootView: list).fittingSize
            XCTAssertLessThan(size.height, Size.popoverMaxHeight / 2, "sample=\(String(describing: sample)): \(size.height)pt")
        }
    }
}
