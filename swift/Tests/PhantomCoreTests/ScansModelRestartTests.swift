// The model's side of the bounded auto-restart (phantom-rrw): a relaunched
// server (possibly on a new port) gets a NEW client and a reload; a spent
// budget or a failed relaunch is the failed screen with a message that
// names the log and Retry.

import XCTest
@testable import PhantomCore

@MainActor
final class ScansModelRestartTests: XCTestCase {

    private func scan(_ root: String) -> Scan {
        Scan(
            id: UUID(), rootPath: root, status: .complete, startedAt: Date(), finishedAt: Date(),
            totalDiskSize: 1, totalLogicalSize: 1, fileCount: 1, dirCount: 1, errorCount: 0,
            unreadablePaths: [], progress: nil
        )
    }

    func testARestartedServerGetsANewClientBuiltFromTheAnnouncedURL() async throws {
        let old = MockAPIClient()
        old.scans = [scan("/old")]
        let fresh = MockAPIClient()
        fresh.scans = [scan("/new")]
        let factoryURLs = Locked<[URL]>([])
        let model = ScansModel(client: old, clientFactory: { url in
            factoryURLs.mutate { $0.append(url) }
            return fresh
        })
        await model.refresh()
        XCTAssertEqual(model.scans.map(\.rootPath), ["/old"])

        let url = URL(string: "http://127.0.0.1:8771")!
        await model.serverRestarted(.success(url))
        XCTAssertEqual(factoryURLs.value, [url], "the client is rebuilt from the NEW announcement")
        XCTAssertEqual(model.connectionState, .connected)
        XCTAssertEqual(model.scans.map(\.rootPath), ["/new"], "and the list is reloaded through it")
    }

    func testACrashLoopIsTheFailedScreenNamingTheLogAndRetry() async {
        let model = ScansModel(client: MockAPIClient())
        await model.serverRestarted(.failure(.crashLoop(exitsInWindow: 4)))
        guard case .failed(let message) = model.connectionState else {
            return XCTFail("expected .failed, got \(model.connectionState)")
        }
        XCTAssertTrue(message.contains("4 times"), message)
        XCTAssertTrue(message.contains("phantom-api.log"), message)
        XCTAssertTrue(message.contains("Retry"), message)
    }

    func testAFailedRelaunchIsTheFailedScreenWithTheReason() async {
        let model = ScansModel(client: MockAPIClient())
        await model.serverRestarted(.failure(.relaunchFailed("timedOut")))
        guard case .failed(let message) = model.connectionState else {
            return XCTFail("expected .failed, got \(model.connectionState)")
        }
        XCTAssertTrue(message.contains("timedOut"), message)
        XCTAssertTrue(message.contains("Retry"), message)
    }
}

/// A tiny lock box so a @Sendable factory can record what it was asked for.
final class Locked<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var _value: T
    init(_ value: T) { _value = value }
    var value: T { lock.lock(); defer { lock.unlock() }; return _value }
    func mutate(_ f: (inout T) -> Void) { lock.lock(); defer { lock.unlock() }; f(&_value) }
}
