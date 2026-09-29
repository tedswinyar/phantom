// The bounded auto-restart rule (phantom-rrw), exhaustively: backoff
// sequence, the budget, the window, and the reset.

import XCTest
@testable import PhantomCore

final class RestartPolicyTests: XCTestCase {

    private let t0 = Date(timeIntervalSince1970: 1_800_000_000)

    func testThreeCrashesAMinuteRestartWithGrowingBackoffThenGiveUp() {
        var p = RestartPolicy.standard
        XCTAssertEqual(p.decide(exitAt: t0), .restart(after: 1))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(5)), .restart(after: 2))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(10)), .restart(after: 4))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(20)), .giveUp(exitsInWindow: 4), "the fourth exit inside the window is a crash loop")
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(21)), .giveUp(exitsInWindow: 5), "and it stays given up while the window is full")
    }

    func testExitsOlderThanTheWindowStopCounting() {
        var p = RestartPolicy(maxRestarts: 2, window: 30, backoff: [1, 2])
        XCTAssertEqual(p.decide(exitAt: t0), .restart(after: 1))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(10)), .restart(after: 2))
        // 31 s after the first exit: it has aged out, so this is the second
        // exit in the window, not the third.
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(31)), .restart(after: 2))
        XCTAssertEqual(p.exitsInWindow(at: t0.addingTimeInterval(31)), 2)
        // A child that stayed up a full window earns a clean slate.
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(100)), .restart(after: 1))
    }

    func testBackoffRepeatsItsLastValue() {
        var p = RestartPolicy(maxRestarts: 5, window: 600, backoff: [1, 3])
        XCTAssertEqual(p.decide(exitAt: t0), .restart(after: 1))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(1)), .restart(after: 3))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(2)), .restart(after: 3))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(3)), .restart(after: 3))
    }

    func testResetForgetsTheHistory() {
        var p = RestartPolicy(maxRestarts: 1, window: 60, backoff: [1])
        XCTAssertEqual(p.decide(exitAt: t0), .restart(after: 1))
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(1)), .giveUp(exitsInWindow: 2))
        p.reset()
        XCTAssertEqual(p.exitsInWindow(at: t0.addingTimeInterval(1)), 0)
        XCTAssertEqual(p.decide(exitAt: t0.addingTimeInterval(2)), .restart(after: 1), "a Retry the user chose starts a fresh budget")
    }
}
