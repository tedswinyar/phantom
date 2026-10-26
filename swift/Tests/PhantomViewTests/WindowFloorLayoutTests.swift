// The window-floor layout gate (phantom-xb1, GitHub tedswinyar/phantom#1).
//
// v1.1.2 crashed on every launch on macOS 27 in windows under ~560pt tall
// once a scan was shown (NSGenericException: more Update Constraints in
// Window passes than there are views), and at 900pt wide and below it laid
// the complete-scan detail out WIDER than the window — sidebar squeezed,
// inspector clipped. Nobody on the project saw either: the dev Mac's saved
// window frame is 3840x2130, and nothing hosted the detail at a small size.
//
// Root of both: the detail's VSplitView is AppKit-backed and reports NO
// minimum size to SwiftUI (its hosting view fits to 0x0), so the HStack
// handed the inspector its full width, the window floor of 720 was a
// fiction, and on macOS 27 the nested split views' min-size handshake
// looped. The fix declares the floor (`Size.detailMinWidth`), makes the
// window floor cover sidebar + detail + inspector, and drops the outer
// HSplitView (Walter Cox's patch). This gate pins the first two by hosting
// the REAL window content at the floor and at the small-screen default.
//
// What it cannot see: the macOS 27 loop itself — AppKit raises it from a
// window's layout pass, and the build server runs macOS 26. The scripts
// suite's test-no-nested-split-views.sh guards that part at the source.
// Headless (NSHostingView, no window, no run loop), so it runs under plain
// `swift test` on the runner like RenderedViewTests.

import XCTest
import AppKit
import SwiftUI
@testable import Phantom
@testable import PhantomCore
import DesignKit

@MainActor
final class WindowFloorLayoutTests: XCTestCase {

    /// A connected model showing one complete scan — the state every launch
    /// lands in once a scan exists (launch auto-selects the newest).
    private func completeScanModel(reclaimEstimate: UInt64? = nil) async throws -> ScansModel {
        let client = MockClientForViews()
        if let reclaimEstimate {
            client.hotspots = HotspotsSummary(
                groups: [], reclaimEstimate: reclaimEstimate, reviewDiskSize: 0,
                cloudDataloadedLogicalSize: 0, cloudDataloadedDiskSize: 0)
        }
        let model = ScansModel(client: client)
        model.connectionState = .connected
        var scan = MockClientForViews.scan("/Users/margaret", day: 1, size: 369_170_000_000)
        // Every real home scan has a few: the callout row is part of the
        // window at its floor.
        scan.errorCount = 3
        scan.unreadablePaths = [
            UnreadablePath(path: "/Users/margaret/Documents", reason: "Operation not permitted (os error 1)"),
            UnreadablePath(path: "/Users/margaret/Library/Containers/x/tmp", reason: "Interrupted system call (os error 4)"),
            UnreadablePath(path: "/Users/margaret/Library/CloudStorage/OneDrive/.Trash", reason: "Operation timed out (os error 60)"),
        ]
        model.scans = [scan]
        await model.select(scanID: scan.id)
        XCTAssertEqual(try XCTUnwrap(model.selectedScan).status, .complete)
        return model
    }

    /// The window sizes the gate hosts at: the floor the window enforces;
    /// the window a 1680x1020pt screen opened (the reporter's MacBook Air,
    /// 900x532 in v1.1.2 — the floor now holds the width, 532 is the floor
    /// height plus the toolbar); and a large window as the control.
    private var windowSizes: [CGSize] {
        [
            CGSize(width: Size.windowMinWidth, height: Size.windowMinHeight),
            CGSize(width: max(900, Size.windowMinWidth), height: 532),
            CGSize(width: 1400, height: 900),
        ]
    }

    /// Every AppKit view laid out beyond its superview's bounds, as
    /// "Child frame in Parent bounds". Stops at scroll views: their pocket
    /// and banner machinery legitimately overhangs, and their content is
    /// meant to exceed the clip.
    private func overflows(in view: NSView) -> [String] {
        if view is NSScrollView { return [] }
        var found: [String] = []
        for child in view.subviews {
            // AppKit's key-view-loop helper, parked off-bounds by design
            // wherever a popover anchor exists; invisible, not layout.
            if String(describing: type(of: child)) == "KeyViewProxy" { continue }
            let f = child.frame, b = view.bounds
            if f.width > 0, f.height > 0,
               f.minX < b.minX - 0.5 || f.minY < b.minY - 0.5
                || f.maxX > b.maxX + 0.5 || f.maxY > b.maxY + 0.5 {
                found.append("\(type(of: child)) \(f) in \(type(of: view)) \(b)")
            }
            found += overflows(in: child)
        }
        return found
    }

    private func host<V: View>(_ view: V, at size: CGSize) -> NSHostingView<some View> {
        let hosting = NSHostingView(rootView: view.frame(width: size.width, height: size.height))
        hosting.frame = CGRect(origin: .zero, size: size)
        hosting.layoutSubtreeIfNeeded()
        return hosting
    }

    /// The tab row is what sets the detail's real minimum (a four-segment
    /// picker, 392pt, plus its padding). Add a fifth tab or a longer title
    /// and this says to raise `Size.detailMinWidth` — and the window floor
    /// with it — instead of letting the row overflow again.
    func testLowerTabRowFitsUnderTheDeclaredDetailFloor() async throws {
        let model = try await completeScanModel()
        let row = NSHostingView(rootView: LowerTabPane().environment(model)).fittingSize
        XCTAssertGreaterThan(row.width, 300, "the tab row measured as (nearly) nothing — the probe is not seeing the segmented control: \(row)")
        XCTAssertLessThanOrEqual(row.width, Size.detailMinWidth,
            "the lower tab row needs \(row.width)pt but Size.detailMinWidth is \(Size.detailMinWidth); raise the token (and check the window floor)")
    }

    /// With a reclaim estimate on the row (every real scan has one; the
    /// 409 mock hides it), the row must stay ONE control tall at the
    /// detail's floor and at the widths a 900x532 window gives it: the
    /// label yields (phrase → number → nothing) instead of wrapping. Make
    /// the estimate a plain `Text` again and the row at 460pt grows to five
    /// lines of "reclaim esti- mate:".
    func testReclaimEstimateYieldsRatherThanWrappingTheTabRow() async throws {
        let plain = try await completeScanModel()
        let oneControlTall = NSHostingView(rootView: LowerTabPane().environment(plain).frame(width: 900)).fittingSize.height
        XCTAssertGreaterThan(oneControlTall, 20, "the bare row measured as nothing: \(oneControlTall)")

        let withEstimate = try await completeScanModel(reclaimEstimate: 137_740_000_000)
        XCTAssertNotNil(withEstimate.hotspots, "the mock must have delivered the estimate for this test to see the label")
        for width in [Size.detailMinWidth, 520, 600, 900] {
            let hosting = NSHostingView(rootView: LowerTabPane().environment(withEstimate).frame(width: width))
            let size = hosting.fittingSize
            XCTAssertLessThanOrEqual(size.height, oneControlTall + 2,
                "tab row with the estimate is \(size.height)pt tall at \(width)pt; the bare row is \(oneControlTall)pt — the label is wrapping")
            hosting.frame = CGRect(origin: .zero, size: size)
            hosting.layoutSubtreeIfNeeded()
            XCTAssertEqual(overflows(in: hosting), [], "tab row at \(width)pt lays out beyond itself")
        }
    }

    /// A throwaway defaults suite so the stored inspector width is the
    /// test's, not whatever the last run or the real app left behind.
    private func freshDefaults(inspectorWidth: Double? = nil) throws -> UserDefaults {
        let name = "WindowFloorLayoutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defaults.removePersistentDomain(forName: name)
        if let inspectorWidth { defaults.set(inspectorWidth, forKey: InspectorWidth.storageKey) }
        return defaults
    }

    /// The detail must TELL SwiftUI its minimum; the AppKit split view will
    /// not. Delete the `.frame(minWidth:)` in CompleteScanDetail and the
    /// fitting width collapses to 0 (inspector hidden) / handle + inspector
    /// (shown).
    func testCompleteScanDetailDeclaresItsMinimumWidth() async throws {
        let model = try await completeScanModel()
        let defaults = try freshDefaults()
        model.showInspector = false
        let alone = NSHostingView(rootView: CompleteScanDetail().environment(model).defaultAppStorage(defaults)).fittingSize
        XCTAssertEqual(alone.width, Size.detailMinWidth, "inspector hidden: the detail's minimum is the declared floor")
        model.showInspector = true
        let withInspector = NSHostingView(rootView: CompleteScanDetail().environment(model).defaultAppStorage(defaults)).fittingSize
        XCTAssertEqual(withInspector.width, Size.detailMinWidth + Size.resizeHandleWidth + Size.inspectorDefaultWidth,
            "inspector shown: floor + the resize handle + the inspector at its default width")
    }

    /// The stored preference drives the inspector's width, clamped to the
    /// band — a default of 100 or 10,000 (a stale or hand-edited plist) lands
    /// on the band's edge, never outside what the window floor accounts for.
    /// Return the proposal unclamped from `InspectorWidth.clamped` and the
    /// first and last rows fail.
    func testInspectorWidthFollowsTheStoredPreferenceWithinTheBand() async throws {
        let model = try await completeScanModel()
        model.showInspector = true
        let total = CGSize(width: 1200, height: 600)
        for (stored, expected) in [(100.0, Size.inspectorMinWidth), (260.0, 260), (10_000.0, Size.inspectorMaxWidth)] {
            let defaults = try freshDefaults(inspectorWidth: stored)
            let hosting = host(CompleteScanDetail().environment(model).defaultAppStorage(defaults), at: total)
            // The only AppKit-backed child at the top is the detail's split
            // view; what it does not get went to the handle and the inspector.
            let splitHost = try XCTUnwrap(hosting.subviews.first, "the detail's split view was not hosted")
            XCTAssertEqual(splitHost.frame.width, total.width - Size.resizeHandleWidth - expected,
                "stored \(stored): the inspector should be \(expected)pt wide")
        }
    }

    func testInspectorWidthClampIsTotal() {
        XCTAssertEqual(InspectorWidth.clamped(-5), Size.inspectorMinWidth)
        XCTAssertEqual(InspectorWidth.clamped(Double(Size.inspectorMinWidth) + 1), Size.inspectorMinWidth + 1)
        XCTAssertEqual(InspectorWidth.clamped(.infinity), Size.inspectorDefaultWidth)
        XCTAssertEqual(InspectorWidth.clamped(.nan), Size.inspectorDefaultWidth)
    }

    /// The whole window content at the floor and at the small-screen
    /// default, inspector shown and hidden: nothing may be laid out beyond
    /// its container. Put `Size.windowMinWidth` back to 720 and the floor
    /// case with the inspector reports the navigation split view at 849pt
    /// inside 720.
    func testNothingOverflowsTheWindowAtItsFloorOrTheSmallScreenDefault() async throws {
        let model = try await completeScanModel(reclaimEstimate: 137_740_000_000)
        for inspector in [true, false] {
            model.showInspector = inspector
            for size in windowSizes {
                let hosting = host(ContentView().environment(model), at: size)
                let found = overflows(in: hosting)
                XCTAssertEqual(found, [],
                    "inspector \(inspector ? "shown" : "hidden") at \(Int(size.width))x\(Int(size.height)): laid out beyond its container — \(found)")
            }
        }
    }
}
