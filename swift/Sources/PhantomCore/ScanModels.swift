// The scan domain's wire models — the Swift twins of rust/phantom-core/src/
// scan.rs (Scan, ScanEntry, TreemapRect/Layout), scanner.rs (ProgressSnapshot),
// and format.rs (FileTypeTotal). Field names map 1:1 to the camelCase JSON
// keys — no CodingKeys remapping, so what you read here is what crosses the
// wire. Pinned by the shared fixtures in tests/fixtures/.
//
// `diskSize` (st_blocks × 512) is THE size everywhere; `logicalSize` is a
// secondary field kept for cloud-dataloaded detection (logical ≫ disk).
//
// Every type with nullable fields hand-writes encode(to:): the synthesized
// encoder uses encodeIfPresent and OMITS nil keys, violating the wire
// contract's present-as-null rule (found by mutation-testing a stamped
// project, 2026-08-19). Each has a
// key-set drift test (ScanWireFormatTests) that goes red if a stored field
// is added without a matching encode line.

import Foundation

/// Lifecycle of a scan. Raw values are the exact wire strings; an unknown
/// status fails the decode rather than mapping to a guess.
public enum ScanStatus: String, Codable, Sendable {
    case running
    case complete
    case cancelled
    case failed
}

/// The wire view of a scan: metadata and totals, plus live `progress` while
/// it runs. `progress` is an object while running and null once terminal —
/// its nil-ness IS the terminal signal the poller watches.
public struct Scan: Codable, Identifiable, Equatable, Sendable {
    /// Live walk counters, present only while the scan runs.
    public struct Progress: Codable, Equatable, Sendable {
        public let filesSeen: UInt64
        /// Disk bytes (st_blocks × 512), consistent with every other total.
        public let bytesSeen: UInt64
        public let currentPath: String

        public init(filesSeen: UInt64, bytesSeen: UInt64, currentPath: String) {
            self.filesSeen = filesSeen
            self.bytesSeen = bytesSeen
            self.currentPath = currentPath
        }
    }

    public let id: UUID
    public let rootPath: String
    public var status: ScanStatus
    public let startedAt: Date
    /// Set when the scan reaches a terminal status. Nullable on the wire,
    /// never absent.
    public var finishedAt: Date?
    /// Sum of file diskSize (st_blocks × 512) — THE headline number.
    public var totalDiskSize: UInt64
    /// Sum of file logical size; secondary, kept for dataloaded detection.
    public var totalLogicalSize: UInt64
    public var fileCount: UInt64
    public var dirCount: UInt64
    /// Entries skipped because they could not be read (permissions, races).
    public var errorCount: UInt64
    /// A capped SAMPLE (first 100, walk order) of the entries behind
    /// `errorCount`, each with the OS's reason — `errorCount` stays the
    /// truth. Empty until a scan completes; `nil` == not recorded (rows
    /// persisted before schema v4), distinct from "recorded, none failed".
    public var unreadablePaths: [UnreadablePath]?
    /// Object while running, null when terminal (nullable-present-as-null).
    public var progress: Progress?

    public var isTerminal: Bool { status != .running }

    public init(
        id: UUID, rootPath: String, status: ScanStatus, startedAt: Date,
        finishedAt: Date?, totalDiskSize: UInt64, totalLogicalSize: UInt64,
        fileCount: UInt64, dirCount: UInt64, errorCount: UInt64,
        unreadablePaths: [UnreadablePath]?, progress: Progress?
    ) {
        self.id = id
        self.rootPath = rootPath
        self.status = status
        self.startedAt = startedAt
        self.finishedAt = finishedAt
        self.totalDiskSize = totalDiskSize
        self.totalLogicalSize = totalLogicalSize
        self.fileCount = fileCount
        self.dirCount = dirCount
        self.errorCount = errorCount
        self.unreadablePaths = unreadablePaths
        self.progress = progress
    }

    enum CodingKeys: String, CodingKey {
        case id, rootPath, status, startedAt, finishedAt
        case totalDiskSize, totalLogicalSize
        case fileCount, dirCount, errorCount, unreadablePaths, progress
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        // Foundation's UUID encodes UPPERCASE via Codable; the wire contract
        // is lowercase out (any case in). Encode the canonical form.
        try c.encode(id.uuidString.lowercased(), forKey: .id)
        try c.encode(rootPath, forKey: .rootPath)
        try c.encode(status, forKey: .status)
        try c.encode(startedAt, forKey: .startedAt)
        if let finishedAt { try c.encode(finishedAt, forKey: .finishedAt) } else { try c.encodeNil(forKey: .finishedAt) }
        try c.encode(totalDiskSize, forKey: .totalDiskSize)
        try c.encode(totalLogicalSize, forKey: .totalLogicalSize)
        try c.encode(fileCount, forKey: .fileCount)
        try c.encode(dirCount, forKey: .dirCount)
        try c.encode(errorCount, forKey: .errorCount)
        if let unreadablePaths { try c.encode(unreadablePaths, forKey: .unreadablePaths) } else { try c.encodeNil(forKey: .unreadablePaths) }
        if let progress { try c.encode(progress, forKey: .progress) } else { try c.encodeNil(forKey: .progress) }
    }
}

/// One entry the walker counted in `errorCount`, with the OS's reason —
/// "Mail is protected" and "the disk is dying" must be distinguishable.
public struct UnreadablePath: Codable, Equatable, Sendable {
    public let path: String
    /// The OS error text, e.g. "Operation not permitted (os error 1)".
    public let reason: String

    public init(path: String, reason: String) {
        self.path = path
        self.reason = reason
    }
}

/// One filesystem entry within a scan. Identified by path — paths are unique
/// within a scan and every read surface is already scoped to one.
public struct ScanEntry: Codable, Identifiable, Equatable, Sendable {
    public var id: String { path }

    public let path: String
    /// `nil` exactly for the scan root; every other entry's parent is inside
    /// the scan.
    public let parentPath: String?
    public let name: String
    public let isDir: Bool
    /// st_blocks × 512. Directory rows carry AGGREGATED subtree sizes on the
    /// read surfaces (tree/entry); descendant counts ride fileCount/dirCount.
    public let diskSize: UInt64
    public let logicalSize: UInt64
    public let modifiedAt: Date?
    /// Lowercased extension; `nil` for directories and extensionless files.
    public let fileType: String?
    /// Reclaimability category. `nil` until Phase 5's classifier fills it.
    public let category: String?
    /// Hardlink metadata for dedup: entries sharing (dev, ino) with nlink > 1
    /// are the same physical file and must be counted once.
    public let nlink: UInt64
    public let dev: UInt64
    public let ino: UInt64
    /// Directory rows: descendant FILES at full depth, counted server-side
    /// from the FULL walk — sub-1-MiB files count even though their rows are
    /// never persisted (counting fetched children is a structural
    /// undercount). `nil` on file rows, and on directory rows persisted
    /// before schema v3.
    public let fileCount: UInt64?
    /// Same contract for descendant DIRECTORIES at full depth, excluding the
    /// entry itself.
    public let dirCount: UInt64?

    /// The headline size for any display or aggregation. Always diskSize —
    /// logicalSize overstates cloud-dataloaded files that occupy ~0 blocks.
    public var displaySize: UInt64 { diskSize }

    public init(
        path: String, parentPath: String?, name: String, isDir: Bool,
        diskSize: UInt64, logicalSize: UInt64, modifiedAt: Date?,
        fileType: String?, category: String?, nlink: UInt64, dev: UInt64,
        ino: UInt64, fileCount: UInt64? = nil, dirCount: UInt64? = nil
    ) {
        self.path = path
        self.parentPath = parentPath
        self.name = name
        self.isDir = isDir
        self.diskSize = diskSize
        self.logicalSize = logicalSize
        self.modifiedAt = modifiedAt
        self.fileType = fileType
        self.category = category
        self.nlink = nlink
        self.dev = dev
        self.ino = ino
        self.fileCount = fileCount
        self.dirCount = dirCount
    }

    enum CodingKeys: String, CodingKey {
        case path, parentPath, name, isDir, diskSize, logicalSize
        case modifiedAt, fileType, category, nlink, dev, ino
        case fileCount, dirCount
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(path, forKey: .path)
        if let parentPath { try c.encode(parentPath, forKey: .parentPath) } else { try c.encodeNil(forKey: .parentPath) }
        try c.encode(name, forKey: .name)
        try c.encode(isDir, forKey: .isDir)
        try c.encode(diskSize, forKey: .diskSize)
        try c.encode(logicalSize, forKey: .logicalSize)
        if let modifiedAt { try c.encode(modifiedAt, forKey: .modifiedAt) } else { try c.encodeNil(forKey: .modifiedAt) }
        if let fileType { try c.encode(fileType, forKey: .fileType) } else { try c.encodeNil(forKey: .fileType) }
        if let category { try c.encode(category, forKey: .category) } else { try c.encodeNil(forKey: .category) }
        try c.encode(nlink, forKey: .nlink)
        try c.encode(dev, forKey: .dev)
        try c.encode(ino, forKey: .ino)
        if let fileCount { try c.encode(fileCount, forKey: .fileCount) } else { try c.encodeNil(forKey: .fileCount) }
        if let dirCount { try c.encode(dirCount, forKey: .dirCount) } else { try c.encodeNil(forKey: .dirCount) }
    }
}

/// A rectangle in the treemap layout.
public struct TreemapRect: Codable, Identifiable, Equatable, Sendable {
    /// A residual shares its parent's `path` (a hit resolves to the parent),
    /// so `path` alone is not unique — the id disambiguates it.
    public var id: String { residual ? path + "#residual" : path }

    public let path: String
    public let name: String
    /// diskSize (aggregated for directories).
    public let size: UInt64
    public let x: Double
    public let y: Double
    public let width: Double
    public let height: Double
    public let depth: Int
    public let isDir: Bool
    public let fileType: String?
    /// True for a synthesized "smaller files" pseudo-tile: the visible
    /// remainder of a directory whose persisted children under-sum its
    /// aggregate (the sub-1-MiB folding made honest). Its `path` is the
    /// PARENT directory's path. Always present on the wire; false for every
    /// real rect. Required on decode — the contract has no absent case.
    public let residual: Bool

    public init(
        path: String, name: String, size: UInt64, x: Double, y: Double,
        width: Double, height: Double, depth: Int, isDir: Bool,
        fileType: String?, residual: Bool = false
    ) {
        self.path = path
        self.name = name
        self.size = size
        self.x = x
        self.y = y
        self.width = width
        self.height = height
        self.depth = depth
        self.isDir = isDir
        self.fileType = fileType
        self.residual = residual
    }

    enum CodingKeys: String, CodingKey {
        case path, name, size, x, y, width, height, depth, isDir, fileType
        case residual
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(path, forKey: .path)
        try c.encode(name, forKey: .name)
        try c.encode(size, forKey: .size)
        try c.encode(x, forKey: .x)
        try c.encode(y, forKey: .y)
        try c.encode(width, forKey: .width)
        try c.encode(height, forKey: .height)
        try c.encode(depth, forKey: .depth)
        try c.encode(isDir, forKey: .isDir)
        if let fileType { try c.encode(fileType, forKey: .fileType) } else { try c.encodeNil(forKey: .fileType) }
        try c.encode(residual, forKey: .residual)
    }
}

/// Treemap layout for a directory (server-side layout over the requested
/// subtree at the requested view size).
public struct TreemapLayout: Codable, Equatable, Sendable {
    public let rootPath: String
    public let totalSize: UInt64
    public let rects: [TreemapRect]

    public init(rootPath: String, totalSize: UInt64, rects: [TreemapRect]) {
        self.rootPath = rootPath
        self.totalSize = totalSize
        self.rects = rects
    }
}

/// Disk usage grouped by file type. `fileType: nil` == no extension.
/// GET /scans/{id}/types answers 409 while the scan is still running.
public struct FileTypeTotal: Codable, Equatable, Sendable {
    public let fileType: String?
    public let diskSize: UInt64
    public let fileCount: UInt64

    public init(fileType: String?, diskSize: UInt64, fileCount: UInt64) {
        self.fileType = fileType
        self.diskSize = diskSize
        self.fileCount = fileCount
    }

    enum CodingKeys: String, CodingKey {
        case fileType, diskSize, fileCount
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        if let fileType { try c.encode(fileType, forKey: .fileType) } else { try c.encodeNil(forKey: .fileType) }
        try c.encode(diskSize, forKey: .diskSize)
        try c.encode(fileCount, forKey: .fileCount)
    }
}

/// One page of GET /scans/{id}/files: a bare JSON array body plus an opaque
/// continuation token from the X-Next-Cursor response header (present only
/// when more rows remain). Client-side pairing, not a wire object.
public struct FilePage: Equatable, Sendable {
    public let files: [ScanEntry]
    public let nextCursor: String?

    public init(files: [ScanEntry], nextCursor: String?) {
        self.files = files
        self.nextCursor = nextCursor
    }
}

/// The wire view of a scan diff (phantom-081) — the Swift twin of
/// rust/phantom-core/src/diff.rs. Positional: `scanA` is "before", `scanB`
/// is "after", and every delta reads B − A. Pinned by
/// tests/fixtures/scan-diff.json. `before`/`after` on a DiffEntry are the
/// one nullable pair (null = the directory was absent from that scan), so
/// DiffEntry hand-writes encode(to:) for present-as-null.
public struct ScanDiff: Codable, Equatable, Sendable {
    public let scanA: UUID
    public let scanB: UUID
    /// When each side started. If scanAStartedAt is LATER than
    /// scanBStartedAt the deltas read reverse-chronologically (deltas are
    /// always B − A); `reversedChronology` flags exactly that case.
    public let scanAStartedAt: Date
    public let scanBStartedAt: Date
    /// `true` when scanA started after scanB (signs are inverted vs "what
    /// changed since the older scan"); `nil` when the order is natural.
    public let reversedChronology: Bool?
    public let rootPath: String
    public let diskDelta: Int64
    public let logicalDelta: Int64
    public let fileCountDelta: Int64
    public let dirCountDelta: Int64
    public let errorCountDelta: Int64
    public let grown: [DiffEntry]
    public let freed: [DiffEntry]

    public init(
        scanA: UUID, scanB: UUID, scanAStartedAt: Date, scanBStartedAt: Date,
        reversedChronology: Bool?, rootPath: String, diskDelta: Int64,
        logicalDelta: Int64, fileCountDelta: Int64, dirCountDelta: Int64,
        errorCountDelta: Int64, grown: [DiffEntry], freed: [DiffEntry]
    ) {
        self.scanA = scanA
        self.scanB = scanB
        self.scanAStartedAt = scanAStartedAt
        self.scanBStartedAt = scanBStartedAt
        self.reversedChronology = reversedChronology
        self.rootPath = rootPath
        self.diskDelta = diskDelta
        self.logicalDelta = logicalDelta
        self.fileCountDelta = fileCountDelta
        self.dirCountDelta = dirCountDelta
        self.errorCountDelta = errorCountDelta
        self.grown = grown
        self.freed = freed
    }

    enum CodingKeys: String, CodingKey {
        case scanA, scanB, scanAStartedAt, scanBStartedAt, reversedChronology
        case rootPath, diskDelta, logicalDelta
        case fileCountDelta, dirCountDelta, errorCountDelta, grown, freed
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(scanA.uuidString.lowercased(), forKey: .scanA)
        try c.encode(scanB.uuidString.lowercased(), forKey: .scanB)
        try c.encode(scanAStartedAt, forKey: .scanAStartedAt)
        try c.encode(scanBStartedAt, forKey: .scanBStartedAt)
        if let reversedChronology { try c.encode(reversedChronology, forKey: .reversedChronology) } else { try c.encodeNil(forKey: .reversedChronology) }
        try c.encode(rootPath, forKey: .rootPath)
        try c.encode(diskDelta, forKey: .diskDelta)
        try c.encode(logicalDelta, forKey: .logicalDelta)
        try c.encode(fileCountDelta, forKey: .fileCountDelta)
        try c.encode(dirCountDelta, forKey: .dirCountDelta)
        try c.encode(errorCountDelta, forKey: .errorCountDelta)
        try c.encode(grown, forKey: .grown)
        try c.encode(freed, forKey: .freed)
    }
}

/// One directory's movement in a diff. `before`/`after` are null when the
/// directory was absent from that scan (created or deleted between them).
public struct DiffEntry: Codable, Equatable, Sendable {
    public let path: String
    public let before: UInt64?
    public let after: UInt64?
    public let delta: Int64

    public init(path: String, before: UInt64?, after: UInt64?, delta: Int64) {
        self.path = path
        self.before = before
        self.after = after
        self.delta = delta
    }

    enum CodingKeys: String, CodingKey {
        case path, before, after, delta
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(path, forKey: .path)
        if let before { try c.encode(before, forKey: .before) } else { try c.encodeNil(forKey: .before) }
        if let after { try c.encode(after, forKey: .after) } else { try c.encodeNil(forKey: .after) }
        try c.encode(delta, forKey: .delta)
    }
}
