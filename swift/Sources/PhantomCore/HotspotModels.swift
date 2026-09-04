// The reclaimable-space wire models — the Swift twins of rust/phantom-core/
// src/classify.rs (HotspotsSummary, HotspotGroup). Field names map 1:1 to
// the camelCase JSON keys; pinned by tests/fixtures/hotspots-summary.json.
//
// `diskSize` here is the hardlink-DEDUPED physical size — what deleting the
// whole group would actually free, THE number. `listedDiskSize` is the naive
// per-entry sum (the "17 GB listed, 5 GB freed" gap made visible), and
// logical sizes exist only to quantify the cloud-dataloaded du-lie.
//
// `command` is the one nullable field, so encode(to:) is hand-written for
// present-as-null like every other nullable-bearing wire type; the key-set
// drift test pins the field list.

import Foundation

/// One group of reclaimable (or review-worthy, or merely informational)
/// entries, produced by the classifier at scan persistence time.
public struct HotspotGroup: Codable, Identifiable, Equatable, Sendable {
    public var id: String { ruleId }

    public let ruleId: String
    public let label: String
    /// The wire category string (e.g. "staleProjectArtifact"). Kept raw —
    /// like ScanEntry.category — so a new server-side category degrades to
    /// the default rendering instead of failing the decode.
    public let category: String
    /// Human advice, purely illustrative — backticks in it are typography,
    /// never semantics. The runnable command is the `command` field.
    public let hint: String
    /// The ONE safe, copy-runnable cleanup command, or nil when none
    /// honestly exists (advice-only rules, mixed-tool caches, and every
    /// review-only or informational category). First-class on the wire —
    /// clients never parse the hint.
    public let command: String?
    /// Hardlink-deduped physical bytes — what deleting the whole group
    /// would actually free. THE headline number.
    public let diskSize: UInt64
    /// Naive per-entry sum; exceeds `diskSize` when hardlinks share blocks.
    public let listedDiskSize: UInt64
    public let logicalSize: UInt64
    public let fileCount: UInt64
    /// Largest hotspot roots, biggest first (server-capped).
    public let topPaths: [String]

    enum CodingKeys: String, CodingKey {
        case ruleId, label, category, hint, command
        case diskSize, listedDiskSize, logicalSize, fileCount, topPaths
    }


    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(ruleId, forKey: .ruleId)
        try c.encode(label, forKey: .label)
        try c.encode(category, forKey: .category)
        try c.encode(hint, forKey: .hint)
        if let command { try c.encode(command, forKey: .command) } else { try c.encodeNil(forKey: .command) }
        try c.encode(diskSize, forKey: .diskSize)
        try c.encode(listedDiskSize, forKey: .listedDiskSize)
        try c.encode(logicalSize, forKey: .logicalSize)
        try c.encode(fileCount, forKey: .fileCount)
        try c.encode(topPaths, forKey: .topPaths)
    }

    /// Cloud placeholders occupy ~0 blocks: deleting frees almost nothing.
    /// The wire already excludes them from the reclaim estimate — render
    /// them as informational, never as a saving.
    public var isCloudDataloaded: Bool { category == "cloudDataloaded" }

    /// Visible but never suggested: unclassified-but-big, or data that a
    /// delete would actually lose.
    public var isReviewOnly: Bool {
        category == "reviewFirst" || category == "wontRegenerate"
    }

    public init(
        ruleId: String, label: String, category: String, hint: String,
        command: String? = nil, diskSize: UInt64, listedDiskSize: UInt64,
        logicalSize: UInt64, fileCount: UInt64, topPaths: [String]
    ) {
        self.ruleId = ruleId
        self.label = label
        self.category = category
        self.hint = hint
        self.command = command
        self.diskSize = diskSize
        self.listedDiskSize = listedDiskSize
        self.logicalSize = logicalSize
        self.fileCount = fileCount
        self.topPaths = topPaths
    }
}

/// Per-scan reclaimable-space summary: GET /scans/{id}/hotspots. Answers
/// 409 while the scan is still running; a terminal scan with nothing stored
/// serves the honest all-empty summary.
public struct HotspotsSummary: Codable, Equatable, Sendable {
    /// Sorted server-side: stale project artifacts first, then by category
    /// priority, then deduped size descending.
    public let groups: [HotspotGroup]
    /// Hardlink-deduped disk across RECLAIMABLE categories only, deduped
    /// globally — a (dev, ino) spanning two groups counts once.
    public let reclaimEstimate: UInt64
    /// Deduped disk across review-only categories — visible, never suggested.
    public let reviewDiskSize: UInt64
    /// The du-lie, quantified: what dataloaded placeholders CLAIM…
    public let cloudDataloadedLogicalSize: UInt64
    /// …versus the blocks they actually occupy.
    public let cloudDataloadedDiskSize: UInt64

    /// The classifier had nothing to say about this scan.
    public var isEmpty: Bool { groups.isEmpty }

    public init(
        groups: [HotspotGroup], reclaimEstimate: UInt64, reviewDiskSize: UInt64,
        cloudDataloadedLogicalSize: UInt64, cloudDataloadedDiskSize: UInt64
    ) {
        self.groups = groups
        self.reclaimEstimate = reclaimEstimate
        self.reviewDiskSize = reviewDiskSize
        self.cloudDataloadedLogicalSize = cloudDataloadedLogicalSize
        self.cloudDataloadedDiskSize = cloudDataloadedDiskSize
    }
}
