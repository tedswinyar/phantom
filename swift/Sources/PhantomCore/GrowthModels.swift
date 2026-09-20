// The growth-over-time wire models — the Swift twins of rust/phantom-core/
// src/growth.rs (Growth, GrowthPoint, GrowthLine, GrowthForecast). Field
// names map 1:1 to the camelCase JSON keys; pinned by tests/fixtures/
// growth.json, the same bytes the Rust tests round-trip.
//
// `points` are the root's completed scans, OLDEST first; each `series`
// line's `values` align with `points` positionally (nil = that scan
// recorded nothing for the breakdown, 0 = recorded as absent). `forecast`
// is nil with fewer than two points. `caveat` travels with the number and
// the History view shows it every time the number shows.
//
// Nullable fields encode present-as-null by hand, like every other
// nullable-bearing wire type; the key-set test pins the field lists.

import Foundation

public struct GrowthPoint: Codable, Identifiable, Equatable, Sendable {
    public var id: UUID { scanId }

    public let scanId: UUID
    public let startedAt: Date
    /// THE size: deduped allocated bytes under the root.
    public let totalDiskSize: UInt64
    /// Nil on scans persisted before the clone-aware walker (they also
    /// counted APFS clones twice — the caveat says so).
    public let totalPrivateSize: UInt64?
    public let fileCount: UInt64

    public init(scanId: UUID, startedAt: Date, totalDiskSize: UInt64, totalPrivateSize: UInt64?, fileCount: UInt64) {
        self.scanId = scanId
        self.startedAt = startedAt
        self.totalDiskSize = totalDiskSize
        self.totalPrivateSize = totalPrivateSize
        self.fileCount = fileCount
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(scanId.uuidString.lowercased(), forKey: .scanId)
        try c.encode(startedAt, forKey: .startedAt)
        try c.encode(totalDiskSize, forKey: .totalDiskSize)
        if let totalPrivateSize { try c.encode(totalPrivateSize, forKey: .totalPrivateSize) } else { try c.encodeNil(forKey: .totalPrivateSize) }
        try c.encode(fileCount, forKey: .fileCount)
    }
}

/// One line of the breakdown. `values[i]` belongs to `points[i]`.
public struct GrowthLine: Codable, Identifiable, Equatable, Sendable {
    public var id: String { key }

    public let key: String
    public let values: [UInt64?]

    public init(key: String, values: [UInt64?]) {
        self.key = key
        self.values = values
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(key, forKey: .key)
        var arr = c.nestedUnkeyedContainer(forKey: .values)
        for v in values {
            if let v { try arr.encode(v) } else { try arr.encodeNil() }
        }
    }
}

public struct GrowthForecast: Codable, Equatable, Sendable {
    /// Always "linear" today; kept raw so a smarter server method decodes.
    public let method: String
    public let pointsUsed: Int
    public let spanDays: Double
    /// Negative when the root is shrinking.
    public let bytesPerDay: Int64
    public let latestBytes: UInt64
    public let availableBytes: UInt64?
    /// Only when growing and the volume is readable.
    public let daysUntilFull: Double?
    public let projectedFullAt: Date?
    public let caveat: String

    public init(method: String, pointsUsed: Int, spanDays: Double, bytesPerDay: Int64, latestBytes: UInt64,
                availableBytes: UInt64?, daysUntilFull: Double?, projectedFullAt: Date?, caveat: String) {
        self.method = method
        self.pointsUsed = pointsUsed
        self.spanDays = spanDays
        self.bytesPerDay = bytesPerDay
        self.latestBytes = latestBytes
        self.availableBytes = availableBytes
        self.daysUntilFull = daysUntilFull
        self.projectedFullAt = projectedFullAt
        self.caveat = caveat
    }

    /// The root is getting bigger.
    public var isGrowing: Bool { bytesPerDay > 0 }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(method, forKey: .method)
        try c.encode(pointsUsed, forKey: .pointsUsed)
        try c.encode(spanDays, forKey: .spanDays)
        try c.encode(bytesPerDay, forKey: .bytesPerDay)
        try c.encode(latestBytes, forKey: .latestBytes)
        if let availableBytes { try c.encode(availableBytes, forKey: .availableBytes) } else { try c.encodeNil(forKey: .availableBytes) }
        if let daysUntilFull { try c.encode(daysUntilFull, forKey: .daysUntilFull) } else { try c.encodeNil(forKey: .daysUntilFull) }
        if let projectedFullAt { try c.encode(projectedFullAt, forKey: .projectedFullAt) } else { try c.encodeNil(forKey: .projectedFullAt) }
        try c.encode(caveat, forKey: .caveat)
    }
}

public struct Growth: Codable, Equatable, Sendable {
    public let rootPath: String
    /// The wire spelling, kept raw: total | category | topLevelDir | extension.
    public let groupBy: String
    /// Oldest first.
    public let points: [GrowthPoint]
    public let series: [GrowthLine]
    public let forecast: GrowthForecast?
    public let note: String

    public init(rootPath: String, groupBy: String, points: [GrowthPoint], series: [GrowthLine], forecast: GrowthForecast?, note: String) {
        self.rootPath = rootPath
        self.groupBy = groupBy
        self.points = points
        self.series = series
        self.forecast = forecast
        self.note = note
    }

    /// The headline line: every point's totalDiskSize, oldest first.
    public var totals: [UInt64] { points.map(\.totalDiskSize) }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(rootPath, forKey: .rootPath)
        try c.encode(groupBy, forKey: .groupBy)
        try c.encode(points, forKey: .points)
        try c.encode(series, forKey: .series)
        if let forecast { try c.encode(forecast, forKey: .forecast) } else { try c.encodeNil(forKey: .forecast) }
        try c.encode(note, forKey: .note)
    }
}

/// The wire values `groupBy` accepts — the app never sends anything else.
public enum GrowthGroupBy: String, CaseIterable, Sendable {
    case total
    case category
    case topLevelDir
    case `extension`

    public var label: String {
        switch self {
        case .total: return "Total"
        case .category: return "Category"
        case .topLevelDir: return "Top-level folders"
        case .`extension`: return "Extension"
        }
    }
}
