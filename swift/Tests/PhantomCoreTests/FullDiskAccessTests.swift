// Full Disk Access detection tests: the canary classification (readable
// wins / denied proves absence / all-missing is no-evidence) and the scan
// callout's four-cell decision matrix. The file probe is the mocked
// boundary; the classification logic runs for real against it.

import XCTest
@testable import PhantomCore

/// The filesystem boundary's mock: scripted canary outcomes; unknown paths
/// read as missing (the honest default for a canary that isn't there).
struct MockFileAccessProbe: FileAccessProbing {
    let results: [String: FileAccessProbeResult]

    func result(forReading path: String) -> FileAccessProbeResult {
        results[path] ?? .missing
    }
}

@MainActor
final class FullDiskAccessTests: XCTestCase {

    private let canaries = ["/c/safari", "/c/mail", "/c/messages"]

    private func status(_ results: [String: FileAccessProbeResult]) -> FullDiskAccess.Status {
        FullDiskAccess.status(probe: MockFileAccessProbe(results: results), canaries: canaries)
    }

    // MARK: - canary classification

    func testAnyReadableCanaryProvesTheGrant() {
        // FDA is all-or-nothing: one readable protected dir means granted,
        // even when another canary reads as denied (a stale TCC edge).
        XCTAssertEqual(status(["/c/safari": .readable]), .granted)
        XCTAssertEqual(
            status(["/c/safari": .denied, "/c/mail": .readable]), .granted,
            "readable evidence wins over a denial"
        )
    }

    func testDeniedCanaryProvesAbsenceWhenNothingReads() {
        XCTAssertEqual(status(["/c/safari": .denied]), .notGranted)
        XCTAssertEqual(
            status(["/c/safari": .missing, "/c/mail": .denied]), .notGranted,
            "a missing canary is skipped, not treated as evidence"
        )
    }

    func testAllCanariesMissingIsUndetermined() {
        // No canary exists: no evidence, no claim — FDA messaging stays off.
        XCTAssertEqual(status([:]), .undetermined)
    }

    // MARK: - the scan-callout matrix (errorCount × grant, all four cells)

    private func makeScan(errorCount: UInt64) -> Scan {
        Scan(
            id: UUID(), rootPath: "/Users/ghost", status: .complete,
            startedAt: Date(), finishedAt: Date(),
            totalDiskSize: 100, totalLogicalSize: 100,
            fileCount: 10, dirCount: 2, errorCount: errorCount, unreadablePaths: [], progress: nil
        )
    }

    private func makeModel(fda results: [String: FileAccessProbeResult]) -> ScansModel {
        let model = ScansModel(
            client: MockAPIClient(),
            fdaProbe: MockFileAccessProbe(results: results.isEmpty ? [:] : results)
        )
        model.refreshFDAStatus()
        return model
    }

    func testErrorsWithoutGrantShowTheHint() {
        let model = makeModel(fda: [FullDiskAccess.canaryPaths[0]: .denied])
        XCTAssertEqual(model.fdaStatus, .notGranted)
        XCTAssertTrue(model.shouldShowFDAHint(for: makeScan(errorCount: 3)))
    }

    func testErrorsWithGrantShowNoHint() {
        // Errors with FDA present have other causes; the copy must not
        // promise the grant fixes them.
        let model = makeModel(fda: [FullDiskAccess.canaryPaths[0]: .readable])
        XCTAssertEqual(model.fdaStatus, .granted)
        XCTAssertFalse(model.shouldShowFDAHint(for: makeScan(errorCount: 3)))
    }

    func testCleanScanWithoutGrantShowsNoHint() {
        // A scan of ~/Code needs no grant — zero errors means the picture
        // is already complete; no callout.
        let model = makeModel(fda: [FullDiskAccess.canaryPaths[0]: .denied])
        XCTAssertFalse(model.shouldShowFDAHint(for: makeScan(errorCount: 0)))
    }

    func testCleanScanWithGrantShowsNoHint() {
        let model = makeModel(fda: [FullDiskAccess.canaryPaths[0]: .readable])
        XCTAssertFalse(model.shouldShowFDAHint(for: makeScan(errorCount: 0)))
    }

    func testUndeterminedStatusNeverClaimsFDA() {
        // Errors but no canary evidence: show the honest error count alone.
        let model = makeModel(fda: [:])
        XCTAssertEqual(model.fdaStatus, .undetermined)
        XCTAssertFalse(model.shouldShowFDAHint(for: makeScan(errorCount: 3)))
    }

    func testRefreshFDAStatusTracksTheProbe() {
        // The status is a snapshot of the LAST probe, re-run on demand
        // (connect, app activation after a settings trip).
        let model = makeModel(fda: [FullDiskAccess.canaryPaths[1]: .denied])
        XCTAssertEqual(model.fdaStatus, .notGranted)
    }
}
