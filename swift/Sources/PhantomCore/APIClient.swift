// APIClient — the only network boundary in the app. Mock THIS protocol in
// tests; never mock URLSession or individual views' data.

import Foundation

public enum APIError: Error, LocalizedError, Equatable {
    case serverUnreachable(String)
    case httpError(status: Int, message: String)
    case decodingFailed(String)

    public var errorDescription: String? {
        switch self {
        case .serverUnreachable(let detail):
            return "Cannot reach the Phantom server: \(detail)"
        case .httpError(let status, let message):
            return "\(message) (\(status))"
        case .decodingFailed(let detail):
            return "Unexpected response from server: \(detail)"
        }
    }
}

public protocol APIClientProtocol: Sendable {
    func health() async throws -> Bool

    // Scans. POST /scans answers 202 with the running scan; cancel answers
    // 202 (accepted) or 409 (already terminal); types answers 409 while the
    // scan is still running. Non-2xx surfaces as APIError.httpError with the
    // real status, so callers branch on it without extra client machinery.
    func startScan(rootPath: String) async throws -> Scan
    func listScans() async throws -> [Scan]
    func getScan(id: UUID) async throws -> Scan
    func cancelScan(id: UUID) async throws -> Scan
    func getTreemap(
        scanID: UUID, root: String?, width: Double?, height: Double?, maxDepth: Int?
    ) async throws -> TreemapLayout
    func getTree(scanID: UUID, path: String?) async throws -> [ScanEntry]
    func getEntry(scanID: UUID, path: String) async throws -> ScanEntry
    func listFiles(
        scanID: UUID, fileType: String?, search: String?, sort: String?,
        limit: Int?, cursor: String?
    ) async throws -> FilePage
    func getTypes(scanID: UUID) async throws -> [FileTypeTotal]
    func getHotspots(scanID: UUID) async throws -> HotspotsSummary
}

public struct APIClient: APIClientProtocol {
    public let baseURL: URL
    private let apiKey: String
    private let session: URLSession

    public init(baseURL: URL, apiKey: String, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.apiKey = apiKey
        self.session = session
    }

    /// Resolve base URL and key the same way the CLI and MCP server do:
    /// PHANTOM_API_URL / PHANTOM_API_KEY / PHANTOM_KEY_FILE env,
    /// falling back to the default port and config-dir key file.
    /// The default base URL when `PHANTOM_API_URL` is unset (or unusable).
    public static let defaultBaseURL = URL(string: "http://127.0.0.1:8768")!

    public static func fromEnvironment() -> APIClient {
        let env = ProcessInfo.processInfo.environment
        // Fail SOFT on a bad override. `PHANTOM_API_URL` is user-controlled;
        // `URL(string:)` returns nil for "" and other junk. Force-unwrapping it
        // turned `export PHANTOM_API_URL=` (empty) into a launch crash
        // (H4). An empty/invalid value is treated as "unset" → default port.
        let base = resolveBaseURL(env["PHANTOM_API_URL"])
        let key = env["PHANTOM_API_KEY"] ?? readKeyFile(
            path: env["PHANTOM_KEY_FILE"]
        ) ?? ""
        return APIClient(baseURL: base, apiKey: key)
    }

    /// Resolve a raw `PHANTOM_API_URL` value to a usable base URL, falling
    /// back to the default for nil/empty/unparseable input. A URL missing a
    /// scheme (e.g. "localhost:8768") is not a usable HTTP base, so it too
    /// falls back rather than producing a scheme-less request that never
    /// connects.
    static func resolveBaseURL(_ raw: String?) -> URL {
        guard let raw else { return defaultBaseURL }
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let url = URL(string: trimmed),
              url.scheme != nil,
              url.host != nil
        else { return defaultBaseURL }
        return url
    }

    public static func readKeyFile(path: String?) -> String? {
        let keyPath: String
        if let path {
            keyPath = path
        } else {
            let home = FileManager.default.homeDirectoryForCurrentUser.path
            keyPath = "\(home)/Library/Application Support/phantom/api_key"
        }
        guard let raw = try? String(contentsOfFile: keyPath, encoding: .utf8) else {
            return nil
        }
        let key = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return key.isEmpty ? nil : key
    }

    // MARK: - Endpoints

    public func health() async throws -> Bool {
        struct Health: Decodable { let status: String }
        let h: Health = try await request("GET", "/health")
        return h.status == "ok"
    }

    // MARK: - Scan endpoints

    public func startScan(rootPath: String) async throws -> Scan {
        struct StartScan: Encodable { let rootPath: String }
        return try await request("POST", "/scans", body: StartScan(rootPath: rootPath))
    }

    public func listScans() async throws -> [Scan] {
        try await request("GET", "/scans")
    }

    public func getScan(id: UUID) async throws -> Scan {
        try await request("GET", "/scans/\(id.uuidString.lowercased())")
    }

    public func cancelScan(id: UUID) async throws -> Scan {
        try await request("POST", "/scans/\(id.uuidString.lowercased())/cancel")
    }

    public func getTreemap(
        scanID: UUID, root: String? = nil, width: Double? = nil,
        height: Double? = nil, maxDepth: Int? = nil
    ) async throws -> TreemapLayout {
        try await request(
            "GET", "/scans/\(scanID.uuidString.lowercased())/treemap",
            query: Self.treemapQuery(root: root, width: width, height: height, maxDepth: maxDepth)
        )
    }

    public func getTree(scanID: UUID, path: String? = nil) async throws -> [ScanEntry] {
        var query: [URLQueryItem] = []
        if let path { query.append(URLQueryItem(name: "path", value: path)) }
        return try await request(
            "GET", "/scans/\(scanID.uuidString.lowercased())/tree", query: query
        )
    }

    public func getEntry(scanID: UUID, path: String) async throws -> ScanEntry {
        try await request(
            "GET", "/scans/\(scanID.uuidString.lowercased())/entry",
            query: [URLQueryItem(name: "path", value: path)]
        )
    }

    public func listFiles(
        scanID: UUID, fileType: String? = nil, search: String? = nil,
        sort: String? = nil, limit: Int? = nil, cursor: String? = nil
    ) async throws -> FilePage {
        let query = Self.filesQuery(
            fileType: fileType, search: search, sort: sort, limit: limit, cursor: cursor
        )
        let (data, response) = try await perform(
            "GET", "/scans/\(scanID.uuidString.lowercased())/files",
            query: query, bodyData: nil
        )
        do {
            let files = try Wire.decoder().decode([ScanEntry].self, from: data)
            return FilePage(files: files, nextCursor: Self.nextCursor(from: response))
        } catch {
            throw APIError.decodingFailed(String(describing: error))
        }
    }

    public func getTypes(scanID: UUID) async throws -> [FileTypeTotal] {
        try await request("GET", "/scans/\(scanID.uuidString.lowercased())/types")
    }

    /// The reclaimable-space summary. 409 while the scan is still running
    /// (same contract as /types); an all-empty summary when the classifier
    /// had nothing to say.
    public func getHotspots(scanID: UUID) async throws -> HotspotsSummary {
        try await request("GET", "/scans/\(scanID.uuidString.lowercased())/hotspots")
    }

    // MARK: - Query construction (internal so tests pin the exact names —
    // a drifted parameter name silently means "unfiltered", not an error)

    static func treemapQuery(
        root: String?, width: Double?, height: Double?, maxDepth: Int?
    ) -> [URLQueryItem] {
        var query: [URLQueryItem] = []
        if let root { query.append(URLQueryItem(name: "root", value: root)) }
        if let width { query.append(URLQueryItem(name: "width", value: String(width))) }
        if let height { query.append(URLQueryItem(name: "height", value: String(height))) }
        if let maxDepth { query.append(URLQueryItem(name: "maxDepth", value: String(maxDepth))) }
        return query
    }

    static func filesQuery(
        fileType: String?, search: String?, sort: String?, limit: Int?, cursor: String?
    ) -> [URLQueryItem] {
        var query: [URLQueryItem] = []
        if let fileType { query.append(URLQueryItem(name: "fileType", value: fileType)) }
        if let search { query.append(URLQueryItem(name: "search", value: search)) }
        if let sort { query.append(URLQueryItem(name: "sort", value: sort)) }
        if let limit { query.append(URLQueryItem(name: "limit", value: String(limit))) }
        if let cursor { query.append(URLQueryItem(name: "cursor", value: cursor)) }
        return query
    }

    /// The pagination continuation token, present only when more rows remain.
    /// Header lookup is case-insensitive per HTTPURLResponse.
    static func nextCursor(from response: HTTPURLResponse) -> String? {
        response.value(forHTTPHeaderField: "X-Next-Cursor")
    }

    // MARK: - Transport

    private func request<T: Decodable>(
        _ method: String, _ path: String, query: [URLQueryItem] = []
    ) async throws -> T {
        try await send(method, path, query: query, bodyData: nil)
    }

    private func request<T: Decodable, B: Encodable>(
        _ method: String, _ path: String, body: B
    ) async throws -> T {
        let data = try Wire.encoder().encode(body)
        return try await send(method, path, query: [], bodyData: data)
    }

    private func send<T: Decodable>(
        _ method: String, _ path: String, query: [URLQueryItem], bodyData: Data?
    ) async throws -> T {
        let (data, _) = try await perform(method, path, query: query, bodyData: bodyData)
        do {
            return try Wire.decoder().decode(T.self, from: data)
        } catch {
            throw APIError.decodingFailed(String(describing: error))
        }
    }

    /// The one place a request is actually issued. Returns the raw bytes and
    /// the response so callers that need headers (pagination) can read them;
    /// any non-2xx has already been mapped to APIError.httpError.
    private func perform(
        _ method: String, _ path: String, query: [URLQueryItem], bodyData: Data?
    ) async throws -> (Data, HTTPURLResponse) {
        var url = baseURL.appending(path: path)
        if !query.isEmpty {
            url.append(queryItems: query)
        }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.setValue(apiKey, forHTTPHeaderField: "X-Api-Key")
        if let bodyData {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = bodyData
        }

        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw APIError.serverUnreachable(error.localizedDescription)
        }

        let http = response as? HTTPURLResponse
        let status = http?.statusCode ?? 0
        guard (200..<300).contains(status), let http else {
            let message = Self.errorMessage(from: data)
            throw APIError.httpError(status: status, message: message)
        }
        return (data, http)
    }

    /// Parse the wire error shape `{"error": "<message>"}` from raw bytes.
    public static func errorMessage(from data: Data) -> String {
        struct WireError: Decodable { let error: String }
        if let decoded = try? JSONDecoder().decode(WireError.self, from: data) {
            return decoded.error
        }
        return "unknown server error"
    }
}
