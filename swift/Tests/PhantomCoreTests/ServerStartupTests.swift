// Tests for the startup once-guard — the highest-risk, previously-uncovered
// path in APIServerManager. These drive ServerStartup directly (the seam the
// process supervisor delegates to) so the continuation-safety guarantees are
// verified without spawning a real server:
//
//  - announce            → the awaiter gets the URL
//  - DOUBLE announce     → resolves ONCE (the C1 crash: two "listening" lines
//                          in one read used to resume the continuation twice)
//  - never-announce exit → the awaiter gets an error, not a hang (H1)
//  - timeout             → the awaiter gets an error, not a hang (H1)

import XCTest
@testable import PhantomCore

final class ServerStartupTests: XCTestCase {
    private let announce = "phantom-api listening on http://127.0.0.1:8768\n"

    func testAnnounceResolvesURL() async throws {
        let startup = ServerStartup()
        await startup.ingest(stdoutChunk: announce) // resolves; @discardableResult
        let url = try await startup.awaitURL()
        XCTAssertEqual(url.port, 8768)
    }

    func testAwaiterSuspendedBeforeAnnounceStillResolves() async throws {
        let startup = ServerStartup()
        // Waiter suspends first; the announce arrives after — the ordering the
        // real pipe handler produces.
        async let waited = startup.awaitURL()
        // Give the awaiter a moment to register its continuation.
        try await Task.sleep(nanoseconds: 20_000_000)
        await startup.ingest(stdoutChunk: announce)
        let url = try await waited
        XCTAssertEqual(url.port, 8768)
    }

    // The C1 crash reproduction: two announcement lines in a SINGLE stdout
    // chunk. The old readabilityHandler called completion on every match and
    // trapped ("tried to resume its continuation more than once"). The actor
    // once-guard must resolve exactly once and never trap.
    func testDoubleAnnounceInOneChunkResolvesOnce() async throws {
        let startup = ServerStartup()
        let doubled = announce + "phantom-api listening on http://127.0.0.1:18301\n"
        let first = await startup.ingest(stdoutChunk: doubled)
        XCTAssertEqual(first?.port, 8768, "first announcement wins")
        // A second chunk (or the same one re-fed) is a no-op — no re-resume.
        let second = await startup.ingest(stdoutChunk: announce)
        XCTAssertNil(second, "already settled; must not resolve again")
        let url = try await startup.awaitURL()
        XCTAssertEqual(url.port, 8768)
    }

    func testNeverAnnounceThenExitResolvesToError() async {
        let startup = ServerStartup()
        await startup.markTerminated() // child died without announcing
        do {
            _ = try await startup.awaitURL()
            XCTFail("expected exitedBeforeAnnouncing, not a hang")
        } catch let error as ServerStartup.StartupError {
            XCTAssertEqual(error, .exitedBeforeAnnouncing)
        } catch {
            XCTFail("unexpected error \(error)")
        }
    }

    func testTimeoutResolvesToError() async {
        let startup = ServerStartup()
        await startup.markTimedOut()
        do {
            _ = try await startup.awaitURL()
            XCTFail("expected timedOut, not a hang")
        } catch let error as ServerStartup.StartupError {
            XCTAssertEqual(error, .timedOut)
        } catch {
            XCTFail("unexpected error \(error)")
        }
    }

    func testLaunchFailureResolvesToError() async {
        let startup = ServerStartup()
        await startup.markLaunchFailed("binary not found")
        do {
            _ = try await startup.awaitURL()
            XCTFail("expected launchFailed")
        } catch let error as ServerStartup.StartupError {
            XCTAssertEqual(error, .launchFailed("binary not found"))
        } catch {
            XCTFail("unexpected error \(error)")
        }
    }

    // Announce first; a later termination (server ran then died) must NOT
    // overwrite the successful result — the awaiter still gets the URL.
    func testAnnounceWinsOverLaterTermination() async throws {
        let startup = ServerStartup()
        await startup.ingest(stdoutChunk: announce)
        await startup.markTimedOut()   // no-op
        await startup.markTerminated() // no-op
        let url = try await startup.awaitURL()
        XCTAssertEqual(url.port, 8768)
    }

    // The first settling event wins: a timeout that fires before any announce
    // makes the attempt fail even if an announce arrives afterwards.
    func testTimeoutBeforeAnnounceWins() async {
        let startup = ServerStartup()
        await startup.markTimedOut()
        let late = await startup.ingest(stdoutChunk: announce)
        XCTAssertNil(late, "settled by timeout; a late announce is ignored")
        do {
            _ = try await startup.awaitURL()
            XCTFail("expected timedOut")
        } catch let error as ServerStartup.StartupError {
            XCTAssertEqual(error, .timedOut)
        } catch {
            XCTFail("unexpected error \(error)")
        }
    }

    // M4: multiple callers share one attempt — every waiter must resolve, not
    // just the first (a single stored continuation would leak the rest).
    func testMultipleConcurrentWaitersAllResolve() async throws {
        let startup = ServerStartup()
        async let a = startup.awaitURL()
        async let b = startup.awaitURL()
        async let c = startup.awaitURL()
        try await Task.sleep(nanoseconds: 20_000_000)
        await startup.ingest(stdoutChunk: announce)
        let ports = try await [a.port, b.port, c.port]
        XCTAssertEqual(ports, [8768, 8768, 8768])
    }

    func testNonAnnouncementChunkDoesNotResolve() async {
        let startup = ServerStartup()
        let result = await startup.ingest(stdoutChunk: "some other log line\nstill booting\n")
        XCTAssertNil(result)
        let settled = await startup.isSettled
        XCTAssertFalse(settled)
    }
}
