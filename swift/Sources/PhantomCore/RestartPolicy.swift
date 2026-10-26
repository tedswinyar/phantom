// RestartPolicy — the bounded auto-restart rule for the API child
// (phantom-rrw). Pure and synchronous so the whole rule is unit-tested; the
// APIServerManager only asks it "the child just died — now what?" and
// sleeps for the answer.
//
// The rule: a crash is retried with growing backoff, at most `maxRestarts`
// times within `window`. A child that then stays up longer than the window
// earns a clean slate. Beyond the budget the answer is `.giveUp`, and the
// app shows the failed screen with its Retry button — a crash loop must
// end in a human's hands, not spin forever against a broken binary.

import Foundation

public struct RestartPolicy: Equatable, Sendable {
    public enum Decision: Equatable, Sendable {
        /// Relaunch after this many seconds.
        case restart(after: TimeInterval)
        /// Too many exits inside the window; stop trying.
        case giveUp(exitsInWindow: Int)
    }

    /// Restarts allowed per window (the first exit is restart 1).
    public let maxRestarts: Int
    /// Exits older than this no longer count against the budget.
    public let window: TimeInterval
    /// Backoff per consecutive restart; the last value repeats.
    public let backoff: [TimeInterval]

    /// Exit timestamps inside the current window, oldest first.
    private var exits: [Date] = []

    public init(maxRestarts: Int = 3, window: TimeInterval = 60, backoff: [TimeInterval] = [1, 2, 4]) {
        precondition(maxRestarts >= 1 && !backoff.isEmpty && window > 0)
        self.maxRestarts = maxRestarts
        self.window = window
        self.backoff = backoff
    }

    /// The default: three restarts a minute, 1 s → 2 s → 4 s.
    public static let standard = RestartPolicy()

    /// Record an unexpected exit at `now` and decide.
    public mutating func decide(exitAt now: Date) -> Decision {
        exits.removeAll { now.timeIntervalSince($0) > window }
        exits.append(now)
        let n = exits.count
        if n > maxRestarts {
            return .giveUp(exitsInWindow: n)
        }
        return .restart(after: backoff[min(n, backoff.count) - 1])
    }

    /// Exits currently counted against the budget (for tests and logs).
    public func exitsInWindow(at now: Date) -> Int {
        exits.filter { now.timeIntervalSince($0) <= window }.count
    }

    /// Forget the history — after a deliberate stop, or a Retry the user
    /// chose, the next crash starts a fresh budget.
    public mutating func reset() {
        exits.removeAll()
    }
}
