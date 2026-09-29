// UI-layer spike (phantom-ozu): can a SwiftPM test target import the app's
// EXECUTABLE target and render real SwiftUI views off-screen? These are
// "semantic snapshot" tests — they render with ImageRenderer and assert
// facts about the pixels (a growing sparkline is drawn in the warning
// colour; a one-point series draws nothing) rather than diffing against a
// reference PNG that breaks on every font or OS change.

import XCTest
import SwiftUI
@testable import Phantom
@testable import PhantomCore
import DesignKit

@MainActor
final class RenderedViewTests: XCTestCase {

    /// Render a view at a fixed size to a bitmap, 1x scale for stable pixel
    /// counts.
    private func render<V: View>(_ view: V, width: CGFloat, height: CGFloat) throws -> CGImage {
        let renderer = ImageRenderer(content: view.frame(width: width, height: height))
        renderer.scale = 1
        renderer.proposedSize = ProposedViewSize(width: width, height: height)
        return try XCTUnwrap(renderer.cgImage, "ImageRenderer produced no image")
    }

    /// Count pixels whose colour is "mostly this channel" — enough to tell
    /// orange/green/grey apart without depending on exact token values.
    private enum Hue { case red, green, other }
    private func histogram(_ image: CGImage) throws -> [Hue: Int] {
        let w = image.width, h = image.height
        var data = [UInt8](repeating: 0, count: w * h * 4)
        let ctx = try XCTUnwrap(CGContext(
            data: &data, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ))
        ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
        var counts: [Hue: Int] = [.red: 0, .green: 0, .other: 0]
        for i in stride(from: 0, to: data.count, by: 4) {
            let (r, g, b, a) = (Int(data[i]), Int(data[i + 1]), Int(data[i + 2]), Int(data[i + 3]))
            guard a > 0 else { continue } // transparent background: not drawn
            if r > g + 40 && r > b + 40 { counts[.red, default: 0] += 1 }
            else if g > r + 40 && g > b + 40 { counts[.green, default: 0] += 1 }
            else { counts[.other, default: 0] += 1 }
        }
        return counts
    }

    func testGrowingSparklineDrawsInTheWarningColourAndShrinkingInSuccess() throws {
        let growing = try histogram(try render(Sparkline(values: [100, 200, 400]), width: 60, height: 16))
        XCTAssertGreaterThan(growing[.red] ?? 0, 20, "a growing series is drawn (orange reads as red-dominant): \(growing)")
        XCTAssertEqual(growing[.green], 0, "and never in the success colour")
        let shrinking = try histogram(try render(Sparkline(values: [400, 200, 100]), width: 60, height: 16))
        XCTAssertGreaterThan(shrinking[.green] ?? 0, 20, "a shrinking series is drawn in green: \(shrinking)")
        XCTAssertEqual(shrinking[.red], 0)
    }

    func testOnePointDrawsNothing() throws {
        let one = try histogram(try render(Sparkline(values: [100]), width: 60, height: 16))
        XCTAssertEqual(one.values.reduce(0, +), 0, "no line for a single point: \(one)")
    }

    /// Renders are deterministic across runs: the same view yields the same
    /// bytes. If this ever flakes, reference-PNG snapshots are off the table.
    func testRenderingIsDeterministic() throws {
        let a = try render(Sparkline(values: [1, 5, 3, 8]), width: 60, height: 16)
        let b = try render(Sparkline(values: [1, 5, 3, 8]), width: 60, height: 16)
        XCTAssertEqual(a.dataProvider?.data as Data?, b.dataProvider?.data as Data?)
    }

    /// The sidebar row at a NARROW width must not truncate the name — the
    /// regression tonight's manual smoke caught ("swin…"). A rendered check
    /// on the accessibility label would not see truncation; the pixel test
    /// can only see that SOMETHING is drawn, so this pins the layout choice
    /// (ViewThatFits drops the sparkline) indirectly: at 150 pt the row has
    /// no red/green line pixels, at 300 pt it does.
    func testNarrowSidebarRowDropsTheSparklineRatherThanTruncating() throws {
        // The exact texts from the smoke: an eight-letter user name and a
        // three-digit GB size. A short fixture ("ghost", "200 B") fits beside
        // the sparkline even at 150 pt and proves nothing — the first thing
        // this spike learned about pixel tests.
        let model = ScansModel(client: MockClientForViews())
        let root = "/Users/margaret"
        let older = MockClientForViews.scan(root, day: 1, size: 369_170_000_000)
        let newer = MockClientForViews.scan(root, day: 2, size: 500_660_000_000) // growing → orange line
        model.scans = [newer, older]
        // 100 pt: the size text alone (~60 pt) plus the dot and spacing
        // leaves no room for a 44 pt sparkline → ViewThatFits must drop it.
        // 300 pt: everything fits → the line is drawn. (150 pt still fits
        // both — the second thing the spike learned: a pixel test needs the
        // width at which the layout actually turns, not "narrow-ish".)
        let narrow = try histogram(try render(ScanRow(scan: newer).environment(model), width: 100, height: 44))
        let wide = try histogram(try render(ScanRow(scan: newer).environment(model), width: 300, height: 44))
        XCTAssertEqual(narrow[.red] ?? 0, 0, "narrow: the sparkline yields to the text: \(narrow)")
        XCTAssertGreaterThan(wide[.red] ?? 0, 10, "wide: the sparkline is drawn: \(wide)")
    }
}

/// The smallest APIClientProtocol stand-in this target needs (MockAPIClient
/// lives in PhantomCoreTests and cannot be shared across test targets).
final class MockClientForViews: APIClientProtocol, @unchecked Sendable {
    static func scan(_ root: String, day: Int, size: UInt64) -> Scan {
        let started = Date(timeIntervalSince1970: TimeInterval(1_800_000_000 + day * 86_400))
        return Scan(id: UUID(), rootPath: root, status: .complete, startedAt: started, finishedAt: started,
                    totalDiskSize: size, totalLogicalSize: size, fileCount: 1, dirCount: 1, errorCount: 0,
                    unreadablePaths: [], progress: nil)
    }
    func health() async throws -> Bool { true }
    func startScan(rootPath: String) async throws -> Scan { fatalError("unused") }
    func listScans() async throws -> [Scan] { [] }
    func getScan(id: UUID) async throws -> Scan { fatalError("unused") }
    func cancelScan(id: UUID) async throws -> Scan { fatalError("unused") }
    func deleteScan(id: UUID) async throws {}
    func getTreemap(scanID: UUID, root: String?, width: Double?, height: Double?, maxDepth: Int?) async throws -> TreemapLayout { throw APIError.httpError(status: 409, message: "still running") }
    func getTree(scanID: UUID, path: String?) async throws -> [ScanEntry] { [] }
    func getEntry(scanID: UUID, path: String) async throws -> ScanEntry { throw APIError.httpError(status: 404, message: "not found") }
    func listFiles(scanID: UUID, fileType: String?, search: String?, sort: String?, limit: Int?, cursor: String?) async throws -> FilePage { FilePage(files: [], nextCursor: nil) }
    func getTypes(scanID: UUID) async throws -> [FileTypeTotal] { [] }
    // Every result endpoint answers the way a server does when it has
    // nothing: 409 (still running) for hotspots, 404 for growth/diff. The
    // model maps these to its "not yet" states, which is what the view
    // tests want to render.
    func getHotspots(scanID: UUID) async throws -> HotspotsSummary { throw APIError.httpError(status: 409, message: "still running") }
    func createPlan(scanID: UUID, maxTier: String) async throws -> ReclaimPlan { fatalError("unused") }
    func planScript(planID: UUID) async throws -> String { "" }
    func getGrowth(root: String, groupBy: String) async throws -> Growth { throw APIError.httpError(status: 404, message: "not found: no completed scans") }
    func getDiff(scanA: UUID, scanB: UUID) async throws -> ScanDiff { throw APIError.httpError(status: 404, message: "not found") }
}
