// Format token tests: every surface renders sizes through Format.size, so
// its behavior (file-style counting, sane units) is pinned here.

import XCTest
@testable import DesignKit

final class FormatTests: XCTestCase {
    func testSizeUsesFileStyleUnits() {
        // .file counting is decimal (Finder-style): 1,000,000 bytes is ~1 MB.
        XCTAssertTrue(Format.size(1_000_000).contains("MB"), "got \(Format.size(1_000_000))")
        XCTAssertTrue(Format.size(3_145_728).contains("MB"), "got \(Format.size(3_145_728))")
        XCTAssertTrue(Format.size(22_548_578_304).contains("GB"), "got \(Format.size(22_548_578_304))")
    }

    func testSizeIsMonotonicAcrossMagnitudes() {
        // Different magnitudes must not collapse to the same rendering.
        XCTAssertNotEqual(Format.size(1_000), Format.size(1_000_000))
        XCTAssertNotEqual(Format.size(1_000_000), Format.size(1_000_000_000))
        XCTAssertFalse(Format.size(0).isEmpty)
    }
}
