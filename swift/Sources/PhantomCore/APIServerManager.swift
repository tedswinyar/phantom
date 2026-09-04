// APIServerManager — supervises the bundled phantom-api process.
//
// Two deliberate choices, both paid-for lessons:
//  1. The child's STDERR goes to a log file the user can find
//     (~/Library/Logs/Phantom/phantom-api.log), not to a pipe nobody
//     reads. A crashing server must leave a note.
//  2. The child's STDOUT is parsed for the bound-port line. The API climbs
//     a port ladder when its default port is busy, so the configured port
//     is a *request*, not a fact — only the report line is the truth.
//
// Startup resolution is the risky part: exactly one of three racing events —
// a parsed announcement, child termination, or a timeout — must settle the
// awaiting caller, exactly once. That once-guard lives in `ServerStartup`
// (an actor, so the guarantee is the compiler's, not a comment's) and is
// unit-tested directly. See ServerStartupTests.

import Foundation
import os

/// Coordinates a single server-startup attempt. Exactly one of the competing
/// events settles the awaiting caller; every later event is ignored. This is
/// the once-guard that makes the `CheckedContinuation` impossible to
/// double-resume (the original crash, C1) and impossible to leak (the
/// never-announce hang, H1: termination and timeout both settle it).
public actor ServerStartup {
    public enum StartupError: Error, Equatable {
        /// No announcement line within the startup window.
        case timedOut
        /// The child exited before announcing a URL.
        case exitedBeforeAnnouncing
        /// The child could not be launched at all.
        case launchFailed(String)
    }

    private var settled: Result<URL, StartupError>?
    // Multiple callers can share one attempt (M4: joiners all await the same
    // startup), so waiters is a LIST — a single stored continuation would let
    // a second joiner clobber the first, leaking it into a permanent hang.
    private var waiters: [CheckedContinuation<URL, Error>] = []

    public init() {}

    /// Await the resolved base URL. Safe to call after the attempt has already
    /// settled (returns immediately) or before (suspends until it does), and
    /// from any number of concurrent callers.
    public func awaitURL() async throws -> URL {
        if let settled { return try settled.get() }
        return try await withCheckedThrowingContinuation { cont in
            self.waiters.append(cont)
        }
    }

    /// Feed a chunk of the child's stdout. Resolves on the FIRST announcement
    /// line and ignores everything after — the once-guard. Returns the URL if
    /// this chunk settled the attempt, nil otherwise (already settled, or no
    /// announcement in this chunk).
    @discardableResult
    public func ingest(stdoutChunk text: String) -> URL? {
        guard settled == nil else { return nil }
        for line in text.split(separator: "\n") {
            if let url = APIServerManager.parseAnnouncement(String(line)) {
                complete(.success(url))
                return url
            }
        }
        return nil
    }

    /// The child exited. No-op if a URL was already announced.
    public func markTerminated() { complete(.failure(.exitedBeforeAnnouncing)) }

    /// The startup window elapsed. No-op if already settled.
    public func markTimedOut() { complete(.failure(.timedOut)) }

    /// The child could not be launched. No-op if already settled.
    public func markLaunchFailed(_ message: String) {
        complete(.failure(.launchFailed(message)))
    }

    public var isSettled: Bool { settled != nil }

    private func complete(_ result: Result<URL, StartupError>) {
        guard settled == nil else { return }
        settled = result
        let mapped = result.mapError { $0 as Error }
        for waiter in waiters { waiter.resume(with: mapped) }
        waiters.removeAll()
    }
}

public final class APIServerManager: @unchecked Sendable {
    public static let shared = APIServerManager()

    private let logger = Logger(subsystem: "com.tedswinyar.phantom", category: "APIServer")
    // Guards ALL mutable state below. reportedBaseURL is read through a locked
    // accessor too — the previous public getter read it lock-free while
    // background handlers wrote it (H3, a real data race @unchecked papered
    // over).
    private let lock = NSLock()
    private var process: Process?
    private var _reportedBaseURL: URL?
    /// The startup attempt currently in flight, if any. Concurrent callers
    /// (a second window's `.task`, a Retry) share it instead of each launching
    /// a rival process or being spuriously failed (M4).
    private var startup: ServerStartup?

    /// Seconds to wait for the announcement before failing the startup.
    private let startupTimeout: TimeInterval

    init(startupTimeout: TimeInterval = 15) {
        self.startupTimeout = startupTimeout
    }

    /// The base URL reported by the server's stdout line, once seen.
    /// Synchronized: safe to read from any thread.
    public var reportedBaseURL: URL? {
        lock.lock()
        defer { lock.unlock() }
        return _reportedBaseURL
    }

    /// Parse the server's announcement line. Returns nil for anything else.
    /// Format: "phantom-api listening on http://127.0.0.1:8768"
    public static func parseAnnouncement(_ line: String) -> URL? {
        let prefix = "phantom-api listening on "
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix(prefix) else { return nil }
        return URL(string: String(trimmed.dropFirst(prefix.count)))
    }

    public static var logFileURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appending(path: "Library/Logs/Phantom/phantom-api.log")
    }

    /// Locate the API binary: bundle first (production), then the Rust
    /// build tree (development), then PATH.
    public static func locateBinary() -> URL? {
        if let bundled = Bundle.main.url(forAuxiliaryExecutable: "phantom-api") {
            return bundled
        }
        var dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        for _ in 0..<10 {
            if FileManager.default.fileExists(
                atPath: dir.appending(path: "rust/Cargo.toml").path
            ) {
                for variant in ["release", "debug"] {
                    let candidate = dir.appending(path: "rust/target/\(variant)/phantom-api")
                    if FileManager.default.isExecutableFile(atPath: candidate.path) {
                        return candidate
                    }
                }
            }
            dir = dir.deletingLastPathComponent()
        }
        let paths = (ProcessInfo.processInfo.environment["PATH"] ?? "").split(separator: ":")
        for p in paths {
            let candidate = URL(fileURLWithPath: String(p)).appending(path: "phantom-api")
            if FileManager.default.isExecutableFile(atPath: candidate.path) {
                return candidate
            }
        }
        return nil
    }

    /// Start the server if not already running and resolve its announced base
    /// URL. Concurrent callers share one attempt. Throws `ServerStartup
    /// .StartupError` if the child never announces (exit, or timeout) or
    /// cannot be launched — the caller always gets an answer, never a hang.
    public func startIfNeeded() async throws -> URL {
        // All lock use is confined to these synchronous helpers — Swift 6
        // forbids holding an NSLock across an `await`, and rightly so.
        switch decideStart() {
        case .cached(let url):
            return url
        case .join(let attempt):
            // A caller that merely joined an in-flight attempt just awaits it,
            // rather than launching a rival process or being spuriously failed
            // (M4).
            return try await attempt.awaitURL()
        case .begin(let attempt):
            launch(attempt: attempt)
            do {
                let url = try await attempt.awaitURL()
                commitSuccess(url)
                return url
            } catch {
                // Startup failed: drop the attempt and tear down a child that
                // may be running-but-silent so we don't leak it or block the
                // next try.
                let hung = abandonStartup()
                if let hung, hung.isRunning { hung.terminate() }
                throw error
            }
        }
    }

    private enum StartDecision {
        case cached(URL)
        case join(ServerStartup)
        case begin(ServerStartup)
    }

    /// Atomically decide, under the lock, whether we already have a URL, an
    /// attempt to join, or should begin a fresh one.
    private func decideStart() -> StartDecision {
        lock.lock()
        defer { lock.unlock() }
        if let url = _reportedBaseURL { return .cached(url) }
        if let inFlight = startup { return .join(inFlight) }
        let attempt = ServerStartup()
        startup = attempt
        return .begin(attempt)
    }

    private func commitSuccess(_ url: URL) {
        lock.lock()
        defer { lock.unlock() }
        _reportedBaseURL = url
        startup = nil
    }

    /// Drop the in-flight attempt and hand back any process to tear down.
    private func abandonStartup() -> Process? {
        lock.lock()
        defer { lock.unlock() }
        startup = nil
        let hung = process
        process = nil
        return hung
    }

    private func storeProcess(_ proc: Process) {
        lock.lock()
        defer { lock.unlock() }
        process = proc
    }

    private func launch(attempt: ServerStartup) {
        guard let binary = Self.locateBinary() else {
            logger.error("phantom-api binary not found; start it manually (make start)")
            Task { await attempt.markLaunchFailed("binary not found; run 'make start'") }
            return
        }

        // stderr → log file (create parent dir; append mode).
        let logURL = Self.logFileURL
        try? FileManager.default.createDirectory(
            at: logURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: nil)
        }
        let logHandle = try? FileHandle(forWritingTo: logURL)
        _ = try? logHandle?.seekToEnd()

        let proc = Process()
        proc.executableURL = binary
        proc.environment = ProcessInfo.processInfo.environment
        proc.standardError = logHandle ?? FileHandle.nullDevice

        let stdout = Pipe()
        proc.standardOutput = stdout
        stdout.fileHandleForReading.readabilityHandler = { [logger] handle in
            let data = handle.availableData
            if data.isEmpty {
                // EOF — nothing more will arrive; release the handler.
                handle.readabilityHandler = nil
                return
            }
            guard let text = String(data: data, encoding: .utf8) else { return }
            // Ingest for the announcement; after the attempt is settled this
            // is a no-op but we KEEP reading so the pipe never fills and
            // deadlocks the child on a >64KB stdout write (H2). The handler
            // stays installed and simply drains subsequent output.
            Task {
                if let url = await attempt.ingest(stdoutChunk: text) {
                    logger.info("phantom-api announced \(url.absoluteString)")
                }
            }
        }

        proc.terminationHandler = { [weak self, logger] p in
            logger.warning(
                "phantom-api exited with status \(p.terminationStatus); see \(logURL.path)"
            )
            self?.lock.lock()
            self?.process = nil
            self?._reportedBaseURL = nil
            self?.lock.unlock()
            // Settle the attempt if it hadn't announced yet — otherwise
            // connect() would hang forever (H1). No-op once announced.
            Task { await attempt.markTerminated() }
        }

        do {
            try proc.run()
            storeProcess(proc)
            logger.info("phantom-api started (pid \(proc.processIdentifier))")
        } catch {
            logger.error("failed to launch phantom-api: \(error.localizedDescription)")
            Task { await attempt.markLaunchFailed(error.localizedDescription) }
            return
        }

        // Startup timeout: a child that launches but never announces (wrong
        // binary, stuck init) must not leave connect() suspended (H1).
        let timeout = startupTimeout
        Task {
            try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
            await attempt.markTimedOut()
        }
    }

    public func stop() {
        lock.lock()
        let proc = process
        process = nil
        _reportedBaseURL = nil
        startup = nil
        lock.unlock()
        if let proc, proc.isRunning { proc.terminate() }
    }
}
