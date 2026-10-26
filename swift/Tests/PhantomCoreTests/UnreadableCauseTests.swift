// Classification of unreadable paths FROM THE RAW REASON STRINGS the walker
// writes (phantom-di7). Every bucket and the no-code fallback are reached.

import XCTest
@testable import PhantomCore

final class UnreadableCauseTests: XCTestCase {
    private func path(_ reason: String) -> UnreadablePath {
        UnreadablePath(path: "/Users/margaret/x", reason: reason)
    }

    func testOsErrorCodeIsParsedFromTheSuffixOnly() {
        XCTAssertEqual(path("Operation not permitted (os error 1)").osErrorCode, 1)
        XCTAssertEqual(path("Operation timed out (os error 60)").osErrorCode, 60)
        XCTAssertNil(path("Operation not permitted").osErrorCode, "no suffix, no code")
        XCTAssertNil(path("weird (os error x)").osErrorCode, "junk after the marker is not a code")
    }

    func testEveryBucketFromTheCodesTheWalkerWrites() {
        // The three reasons from a real home scan on 2026-10-25, plus the
        // neighbours each bucket must also catch.
        XCTAssertEqual(path("Operation not permitted (os error 1)").cause, .protected)
        XCTAssertEqual(path("Permission denied (os error 13)").cause, .protected)
        XCTAssertEqual(path("Interrupted system call (os error 4)").cause, .transient)
        XCTAssertEqual(path("No such file or directory (os error 2)").cause, .transient)
        XCTAssertEqual(path("Resource temporarily unavailable (os error 35)").cause, .transient)
        XCTAssertEqual(path("Operation timed out (os error 60)").cause, .timedOut)
        XCTAssertEqual(path("Input/output error (os error 5)").cause, .other, "a dying disk is never filed as protected or transient")
        XCTAssertEqual(path("Something new (os error 999)").cause, .other)
    }

    func testWithoutACodeTheWordsDecide() {
        XCTAssertEqual(path("Operation not permitted").cause, .protected)
        XCTAssertEqual(path("permission denied").cause, .protected)
        XCTAssertEqual(path("Operation timed out").cause, .timedOut)
        XCTAssertEqual(path("Interrupted system call").cause, .transient)
        XCTAssertEqual(path("No such file or directory").cause, .transient)
        XCTAssertEqual(path("unknown failure").cause, .other)
    }

    func testGroupingKeepsCauseOrderAndWalkOrderAndDropsEmptyBuckets() {
        let sample = [
            UnreadablePath(path: "/a", reason: "Operation timed out (os error 60)"),
            UnreadablePath(path: "/b", reason: "Operation not permitted (os error 1)"),
            UnreadablePath(path: "/c", reason: "Permission denied (os error 13)"),
            UnreadablePath(path: "/d", reason: "Input/output error (os error 5)"),
        ]
        XCTAssertEqual(UnreadableGroup.group(sample), [
            UnreadableGroup(cause: .protected, paths: [sample[1], sample[2]]),
            UnreadableGroup(cause: .timedOut, paths: [sample[0]]),
            UnreadableGroup(cause: .other, paths: [sample[3]]),
        ])
        XCTAssertEqual(UnreadableGroup.group([]), [])
    }
}
