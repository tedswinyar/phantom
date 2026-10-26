// Why an entry could not be read, from the OS error the walker recorded.
// The count alone ("6 items could not be read") was a dead end for users
// (phantom-di7): the reasons were on the wire and nowhere in the app. This
// is the model half — classification and grouping, tested from the raw
// reason strings; the copy and the popover live in the app target.

import Foundation

extension UnreadablePath {
    /// Coarse cause, coarse on purpose: each case gets ONE next-step
    /// sentence in the UI. "Mail is protected" and "the disk is dying"
    /// must land in different buckets; two flavours of protected need not.
    public enum Cause: String, CaseIterable, Sendable, Equatable {
        /// macOS refused: EPERM / EACCES — a protected folder, another
        /// user's files, a sandbox container, or a missing Full Disk Access
        /// grant.
        case protected
        /// The entry changed or vanished while the walk passed it, or the
        /// read was interrupted: ENOENT / EINTR / EAGAIN. Harmless.
        case transient
        /// A cloud or network location did not answer: ETIMEDOUT.
        case timedOut
        /// Everything else, including I/O errors — the OS reason is shown.
        case other
    }

    /// The `(os error N)` suffix the walker appends to the OS reason.
    public var osErrorCode: Int? {
        guard let open = reason.range(of: "(os error ", options: .backwards),
              let close = reason[open.upperBound...].firstIndex(of: ")") else { return nil }
        return Int(reason[open.upperBound..<close])
    }

    public var cause: Cause {
        switch osErrorCode {
        case 1, 13: return .protected        // EPERM, EACCES
        case 2, 4, 35: return .transient     // ENOENT, EINTR, EAGAIN
        case 60: return .timedOut            // ETIMEDOUT
        case .some: return .other
        case .none:
            // No code (a reason from another producer): read the words.
            let text = reason.lowercased()
            if text.contains("not permitted") || text.contains("permission denied") { return .protected }
            if text.contains("timed out") { return .timedOut }
            if text.contains("interrupted") || text.contains("no such file") { return .transient }
            return .other
        }
    }
}

/// The sample, bucketed by cause in `Cause.allCases` order, empty buckets
/// dropped, walk order kept inside each bucket.
public struct UnreadableGroup: Equatable, Sendable {
    public let cause: UnreadablePath.Cause
    public let paths: [UnreadablePath]

    public init(cause: UnreadablePath.Cause, paths: [UnreadablePath]) {
        self.cause = cause
        self.paths = paths
    }

    public static func group(_ sample: [UnreadablePath]) -> [UnreadableGroup] {
        UnreadablePath.Cause.allCases.compactMap { cause in
            let paths = sample.filter { $0.cause == cause }
            return paths.isEmpty ? nil : UnreadableGroup(cause: cause, paths: paths)
        }
    }
}
