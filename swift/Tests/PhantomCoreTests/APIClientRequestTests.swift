// The RAW requests APIClient issues (phantom-b7u). MockAPIClient stands in
// for the network everywhere else, so nothing else in the suite would notice
// a wrong path, a snake_case body key, a missing X-Api-Key, or an uppercase
// UUID in a URL. A URLProtocol stub captures each URLRequest and hands back
// the shared fixtures, so this test drives the real encoder, the real URL
// construction and the real error mapping.

import XCTest
@testable import PhantomCore

/// Captures every request the session sends and answers with a scripted
/// (status, body). One class-level queue: the tests here are serial.
final class StubURLProtocol: URLProtocol {
    nonisolated(unsafe) static var captured: [URLRequest] = []
    nonisolated(unsafe) static var responses: [(status: Int, body: Data)] = []
    private static let lock = NSLock()

    static func reset(answering responses: [(Int, Data)]) {
        lock.lock(); defer { lock.unlock() }
        captured = []
        self.responses = responses.map { (status: $0.0, body: $0.1) }
    }

    static func take() -> [URLRequest] {
        lock.lock(); defer { lock.unlock() }
        return captured
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        Self.lock.lock()
        // httpBody is not preserved through URLSession's copy; the stream is.
        var req = request
        if req.httpBody == nil, let stream = req.httpBodyStream {
            stream.open()
            var data = Data()
            let buf = UnsafeMutablePointer<UInt8>.allocate(capacity: 4096)
            defer { buf.deallocate() }
            while stream.hasBytesAvailable {
                let n = stream.read(buf, maxLength: 4096)
                if n <= 0 { break }
                data.append(buf, count: n)
            }
            stream.close()
            req.httpBody = data
        }
        Self.captured.append(req)
        let answer = Self.responses.isEmpty ? (status: 200, body: Data("{}".utf8)) : Self.responses.removeFirst()
        Self.lock.unlock()
        let http = HTTPURLResponse(url: request.url!, statusCode: answer.status, httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: http, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: answer.body)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

final class APIClientRequestTests: XCTestCase {
    private let base = URL(string: "http://127.0.0.1:18770")!
    private let scanID = UUID(uuidString: "E7AE86E2-308B-444C-8A3D-CD21467AB442")! // uppercase on purpose

    private func client() -> APIClient {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [StubURLProtocol.self]
        return APIClient(baseURL: base, apiKey: "test-key-not-secret", session: URLSession(configuration: config))
    }

    private func fixture(_ name: String) throws -> Data { try WireFormatTests.fixture(name) }

    private func bodyKeys(_ req: URLRequest) throws -> Set<String> {
        let obj = try XCTUnwrap(try JSONSerialization.jsonObject(with: try XCTUnwrap(req.httpBody)) as? [String: Any])
        return Set(obj.keys)
    }

    private func query(_ req: URLRequest) -> [String: String] {
        let items = URLComponents(url: req.url!, resolvingAgainstBaseURL: false)?.queryItems ?? []
        return Dictionary(uniqueKeysWithValues: items.map { ($0.name, $0.value ?? "") })
    }

    func testStartScanPostsTheCamelCaseBodyWithTheKeyHeader() async throws {
        StubURLProtocol.reset(answering: [(202, try fixture("scan-running.json"))])
        _ = try await client().startScan(rootPath: "/Users/ghost/Code")
        let req = try XCTUnwrap(StubURLProtocol.take().first)
        XCTAssertEqual(req.httpMethod, "POST")
        XCTAssertEqual(req.url?.path, "/scans")
        XCTAssertEqual(req.value(forHTTPHeaderField: "X-Api-Key"), "test-key-not-secret")
        XCTAssertEqual(req.value(forHTTPHeaderField: "Content-Type"), "application/json")
        // Provenance (phantom-cnr.7): the API log names the app as the sender.
        XCTAssertEqual(req.value(forHTTPHeaderField: "User-Agent"), APIClient.userAgent)
        XCTAssertTrue(APIClient.userAgent.hasPrefix("Phantom/"), APIClient.userAgent)
        XCTAssertEqual(try bodyKeys(req), ["rootPath"], "the wire key, not root_path, and nothing extra")
    }

    func testUUIDsGoLowercaseIntoPathsAndPlanBodyCarriesOnlyMaxTier() async throws {
        StubURLProtocol.reset(answering: [(201, try fixture("reclaim-plan.json"))])
        _ = try await client().createPlan(scanID: scanID, maxTier: "caution")
        let req = try XCTUnwrap(StubURLProtocol.take().first)
        XCTAssertEqual(req.httpMethod, "POST")
        XCTAssertEqual(req.url?.path, "/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/plan", "lowercase out")
        XCTAssertEqual(try bodyKeys(req), ["maxTier"])
        let obj = try JSONSerialization.jsonObject(with: try XCTUnwrap(req.httpBody)) as? [String: Any]
        XCTAssertEqual(obj?["maxTier"] as? String, "caution")
    }

    func testQueryParametersUseTheWireNames() async throws {
        StubURLProtocol.reset(answering: [
            (200, Data("[]".utf8)),
            (200, try fixture("growth.json")),
            (200, try fixture("treemap.json")),
            (200, try fixture("scan-diff.json")),
        ])
        let c = client()
        _ = try await c.listFiles(scanID: scanID, fileType: "mov", search: "a b&c", sort: "size", limit: 50, cursor: "abc")
        _ = try await c.getGrowth(root: "/Users/ghost", groupBy: "topLevelDir")
        _ = try await c.getTreemap(scanID: scanID, root: "/Users/ghost/Code", width: 800, height: 600, maxDepth: 3)
        _ = try await c.getDiff(scanA: scanID, scanB: scanID)
        let reqs = StubURLProtocol.take()
        XCTAssertEqual(reqs.count, 4)
        XCTAssertEqual(reqs[0].url?.path, "/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/files")
        XCTAssertEqual(query(reqs[0]), ["fileType": "mov", "search": "a b&c", "sort": "size", "limit": "50", "cursor": "abc"])
        XCTAssertFalse(reqs[0].url!.absoluteString.contains("a b"), "the space is percent-encoded on the wire")
        XCTAssertEqual(reqs[1].url?.path, "/scans/series")
        XCTAssertEqual(query(reqs[1]), ["root": "/Users/ghost", "groupBy": "topLevelDir"])
        XCTAssertEqual(query(reqs[2]), ["root": "/Users/ghost/Code", "width": "800.0", "height": "600.0", "maxDepth": "3"])
        XCTAssertEqual(reqs[3].url?.path, "/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/diff/e7ae86e2-308b-444c-8a3d-cd21467ab442")
        for r in reqs {
            XCTAssertEqual(r.httpMethod, "GET")
            XCTAssertNil(r.httpBody, "GETs carry no body")
        }
    }

    func testDeleteSendsNoBodyAndTreats204AsSuccess() async throws {
        StubURLProtocol.reset(answering: [(204, Data())])
        try await client().deleteScan(id: scanID)
        let req = try XCTUnwrap(StubURLProtocol.take().first)
        XCTAssertEqual(req.httpMethod, "DELETE")
        XCTAssertEqual(req.url?.path, "/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442")
        XCTAssertNil(req.httpBody)
    }

    func testErrorBodiesBecomeHTTPErrorsWithTheServersMessage() async throws {
        StubURLProtocol.reset(answering: [(409, Data("{\"error\":\"scan is still running\"}".utf8))])
        do {
            _ = try await client().getHotspots(scanID: scanID)
            XCTFail("expected a thrown error")
        } catch let APIError.httpError(status, message) {
            XCTAssertEqual(status, 409)
            XCTAssertEqual(message, "scan is still running")
        }
        // A non-JSON error body is still an HTTP error with a stand-in message.
        StubURLProtocol.reset(answering: [(500, Data("<html>boom</html>".utf8))])
        do {
            _ = try await client().listScans()
            XCTFail("expected a thrown error")
        } catch let APIError.httpError(status, message) {
            XCTAssertEqual(status, 500)
            XCTAssertEqual(message, "unknown server error")
        }
        // A 2xx with a body that is not the type is a decoding failure, not a crash.
        StubURLProtocol.reset(answering: [(200, Data("{\"unexpected\":true}".utf8))])
        do {
            _ = try await client().getScan(id: scanID)
            XCTFail("expected a thrown error")
        } catch APIError.decodingFailed {
            // expected
        }
    }
}
