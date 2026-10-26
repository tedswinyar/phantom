// The reclaimable-space wire models — the Swift twins of rust/phantom-core/
// src/classify.rs (HotspotsSummary, HotspotGroup). Field names map 1:1 to
// the camelCase JSON keys; pinned by tests/fixtures/hotspots-summary.json.
//
// `diskSize` here is the hardlink-DEDUPED physical size — what deleting the
// whole group would actually free, THE number. `listedDiskSize` is the naive
// per-entry sum (the "17 GB listed, 5 GB freed" gap made visible), and
// logical sizes exist only to quantify the cloud-dataloaded du-lie.
//
// `command` and `toolEstimate` are the nullable fields, so encode(to:) is
// hand-written for present-as-null like every other nullable-bearing wire
// type; the key-set drift test pins the field list.
//
// v1.1 (Phase 2) adds the honesty fields: `riskTier` (safe | caution |
// review), a one-sentence `why`, `rebuildCost` and the opt-in
// `toolEstimate`. The strings stay raw (like `category`) so a new server
// value degrades to default rendering instead of failing the decode.

import Foundation

/// What getting a group's bytes back costs: `download`, `compile`, or
/// `none` (nothing to rebuild — or nothing can; the tier says which).
public struct RebuildCost: Codable, Equatable, Sendable {
    public let kind: String
    public let estimate: String

    public init(kind: String, estimate: String) {
        self.kind = kind
        self.estimate = estimate
    }
}

/// The owning tool's OWN dry-run number for a group (`brew cleanup -n`,
/// `docker system df`, `uv cache size`), present only when the scan opted
/// in and the tool was found at a fixed install path.
public struct ToolEstimate: Codable, Equatable, Sendable {
    public let tool: String
    public let command: String
    public let reclaimableBytes: UInt64
    public let note: String

    public init(tool: String, command: String, reclaimableBytes: UInt64, note: String) {
        self.tool = tool
        self.command = command
        self.reclaimableBytes = reclaimableBytes
        self.note = note
    }
}

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
    /// How risky deleting this group is: "safe", "caution" or "review".
    /// Kept raw like `category`; `isSafe` / `isCaution` / `isReviewTier`
    /// are the typed reads. A summary persisted before v1.1 decodes as
    /// "review" (unrated), never as safe.
    public let riskTier: String
    /// One sentence: what this is, what brings it back, what the tier rests
    /// on (lockfile present or missing, project dormant). Shown on hover.
    public let why: String
    public let rebuildCost: RebuildCost
    /// The tool's own dry-run number, or nil (present-as-null on the wire).
    public let toolEstimate: ToolEstimate?
    /// Hardlink-deduped physical bytes — what deleting the whole group
    /// would actually free. THE headline number.
    public let diskSize: UInt64
    /// Naive per-entry sum; exceeds `diskSize` when hardlinks share blocks.
    public let listedDiskSize: UInt64
    /// What deleting the group's paths would ACTUALLY free: a hardlink or
    /// clone group counts only if every reference to it is inside the
    /// group; files held by a snapshot or cloned elsewhere contribute ~0.
    /// ≤ `diskSize`. THE number to promise a user (v1.1).
    public let privateSize: UInt64
    public let logicalSize: UInt64
    public let fileCount: UInt64
    /// Largest hotspot roots, biggest first (server-capped).
    public let topPaths: [String]

    enum CodingKeys: String, CodingKey {
        case ruleId, label, category, hint, command
        case riskTier, why, rebuildCost, toolEstimate
        case diskSize, listedDiskSize, privateSize, logicalSize, fileCount, topPaths
    }

    /// Decode generously: the v1.1 fields default when absent (a summary
    /// persisted by a pre-v1.1 server) — to UNRATED, never to safe.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        ruleId = try c.decode(String.self, forKey: .ruleId)
        label = try c.decode(String.self, forKey: .label)
        category = try c.decode(String.self, forKey: .category)
        hint = try c.decode(String.self, forKey: .hint)
        command = try c.decodeIfPresent(String.self, forKey: .command)
        riskTier = try c.decodeIfPresent(String.self, forKey: .riskTier) ?? "review"
        why = try c.decodeIfPresent(String.self, forKey: .why)
            ?? "classified before v1.1; tier not assessed — rescan for a rating"
        rebuildCost = try c.decodeIfPresent(RebuildCost.self, forKey: .rebuildCost)
            ?? RebuildCost(kind: "none", estimate: "not assessed (classified before v1.1)")
        toolEstimate = try c.decodeIfPresent(ToolEstimate.self, forKey: .toolEstimate)
        diskSize = try c.decode(UInt64.self, forKey: .diskSize)
        listedDiskSize = try c.decode(UInt64.self, forKey: .listedDiskSize)
        privateSize = try c.decodeIfPresent(UInt64.self, forKey: .privateSize) ?? 0
        logicalSize = try c.decode(UInt64.self, forKey: .logicalSize)
        fileCount = try c.decode(UInt64.self, forKey: .fileCount)
        topPaths = try c.decode([String].self, forKey: .topPaths)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(ruleId, forKey: .ruleId)
        try c.encode(label, forKey: .label)
        try c.encode(category, forKey: .category)
        try c.encode(hint, forKey: .hint)
        if let command { try c.encode(command, forKey: .command) } else { try c.encodeNil(forKey: .command) }
        try c.encode(riskTier, forKey: .riskTier)
        try c.encode(why, forKey: .why)
        try c.encode(rebuildCost, forKey: .rebuildCost)
        if let toolEstimate { try c.encode(toolEstimate, forKey: .toolEstimate) } else { try c.encodeNil(forKey: .toolEstimate) }
        try c.encode(diskSize, forKey: .diskSize)
        try c.encode(listedDiskSize, forKey: .listedDiskSize)
        try c.encode(privateSize, forKey: .privateSize)
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

    /// Downloaded model weights: reclaimable, but gigabytes to fetch again.
    public var isModelCache: Bool { category == "modelCache" }

    public var isSafe: Bool { riskTier == "safe" }
    public var isCaution: Bool { riskTier == "caution" }
    /// Also true for an unknown tier string: unfamiliar ≠ safe.
    public var isReviewTier: Bool { !isSafe && !isCaution }

    public init(
        ruleId: String, label: String, category: String, hint: String,
        command: String? = nil, riskTier: String = "review", why: String = "",
        rebuildCost: RebuildCost = RebuildCost(kind: "none", estimate: ""),
        toolEstimate: ToolEstimate? = nil,
        diskSize: UInt64, listedDiskSize: UInt64,
        privateSize: UInt64? = nil, logicalSize: UInt64, fileCount: UInt64,
        topPaths: [String]
    ) {
        self.ruleId = ruleId
        self.label = label
        self.category = category
        self.hint = hint
        self.command = command
        self.riskTier = riskTier
        self.why = why
        self.rebuildCost = rebuildCost
        self.toolEstimate = toolEstimate
        self.diskSize = diskSize
        self.listedDiskSize = listedDiskSize
        // Callers that predate v1.1 promise the du-model size; the wire
        // always carries the real number.
        self.privateSize = privateSize ?? diskSize
        self.logicalSize = logicalSize
        self.fileCount = fileCount
        self.topPaths = topPaths
    }
}

/// One hotspot root directly inside a project (v1.1 Phase 3).
public struct ProjectArtifact: Codable, Equatable, Sendable {
    public let ruleId: String
    public let path: String
    public let category: String
    /// Like HotspotGroup: an absent or unknown tier decodes as "review" —
    /// unfamiliar is never safe.
    public let riskTier: String
    public let diskSize: UInt64

    enum CodingKeys: String, CodingKey {
        case ruleId, path, category, riskTier, diskSize
    }

    public init(ruleId: String, path: String, category: String, riskTier: String, diskSize: UInt64) {
        self.ruleId = ruleId
        self.path = path
        self.category = category
        self.riskTier = riskTier
        self.diskSize = diskSize
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        ruleId = try c.decode(String.self, forKey: .ruleId)
        path = try c.decode(String.self, forKey: .path)
        category = try c.decode(String.self, forKey: .category)
        riskTier = try c.decodeIfPresent(String.self, forKey: .riskTier) ?? "review"
        diskSize = try c.decode(UInt64.self, forKey: .diskSize)
    }
}

/// What the staleness rule saw for one project root, recorded at scan time
/// (v1.1 Phase 3). `lastActivityDays` nil == unverifiable, never stale.
public struct ProjectActivity: Codable, Equatable, Sendable {
    public let root: String
    public let lastActivityDays: Int64?
    public let dormant: Bool
    public let artifacts: [ProjectArtifact]

    enum CodingKeys: String, CodingKey {
        case root, lastActivityDays, dormant, artifacts
    }

    public init(root: String, lastActivityDays: Int64?, dormant: Bool, artifacts: [ProjectArtifact]) {
        self.root = root
        self.lastActivityDays = lastActivityDays
        self.dormant = dormant
        self.artifacts = artifacts
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(root, forKey: .root)
        if let lastActivityDays { try c.encode(lastActivityDays, forKey: .lastActivityDays) } else { try c.encodeNil(forKey: .lastActivityDays) }
        try c.encode(dormant, forKey: .dormant)
        try c.encode(artifacts, forKey: .artifacts)
    }
}

/// Per-scan reclaimable-space summary: GET /scans/{id}/hotspots. Answers
/// 409 while the scan is still running; a terminal scan with nothing stored
/// serves the honest all-empty summary.
public struct HotspotsSummary: Codable, Equatable, Sendable {
    /// Sorted server-side: stale project artifacts first, then by category
    /// priority, then deduped size descending.
    public let groups: [HotspotGroup]
    /// What deleting every reclaimable hotspot would actually free: Σ
    /// `privateSize` across the reclaimable categories, settled
    /// globally (a sharing group counts once, and only if every reference
    /// to it is inside the reclaimable set). Since v1.1 this is NOT Σ
    /// diskSize — a Finder-duplicated project's `node_modules` reclaims ~0.
    public let reclaimEstimate: UInt64
    /// Deduped disk across review-only categories — visible, never suggested.
    public let reviewDiskSize: UInt64
    /// The du-lie, quantified: what dataloaded placeholders CLAIM…
    public let cloudDataloadedLogicalSize: UInt64
    /// …versus the blocks they actually occupy.
    public let cloudDataloadedDiskSize: UInt64
    /// Every project root the staleness rule evaluated (v1.1 Phase 3);
    /// empty for summaries persisted before it (the key may be absent).
    public let projects: [ProjectActivity]

    /// The classifier had nothing to say about this scan.
    public var isEmpty: Bool { groups.isEmpty }

    public init(
        groups: [HotspotGroup], reclaimEstimate: UInt64, reviewDiskSize: UInt64,
        cloudDataloadedLogicalSize: UInt64, cloudDataloadedDiskSize: UInt64,
        projects: [ProjectActivity] = []
    ) {
        self.groups = groups
        self.reclaimEstimate = reclaimEstimate
        self.reviewDiskSize = reviewDiskSize
        self.cloudDataloadedLogicalSize = cloudDataloadedLogicalSize
        self.cloudDataloadedDiskSize = cloudDataloadedDiskSize
        self.projects = projects
    }

    enum CodingKeys: String, CodingKey {
        case groups, reclaimEstimate, reviewDiskSize, cloudDataloadedLogicalSize, cloudDataloadedDiskSize, projects
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        groups = try c.decode([HotspotGroup].self, forKey: .groups)
        reclaimEstimate = try c.decode(UInt64.self, forKey: .reclaimEstimate)
        reviewDiskSize = try c.decode(UInt64.self, forKey: .reviewDiskSize)
        cloudDataloadedLogicalSize = try c.decode(UInt64.self, forKey: .cloudDataloadedLogicalSize)
        cloudDataloadedDiskSize = try c.decode(UInt64.self, forKey: .cloudDataloadedDiskSize)
        // Absent in pre-Phase-3 bodies: decode as none, never fail the summary.
        projects = try c.decodeIfPresent([ProjectActivity].self, forKey: .projects) ?? []
    }
}
