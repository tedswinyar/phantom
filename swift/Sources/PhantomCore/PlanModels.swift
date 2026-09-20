// Reclaim-plan wire models (v1.1 Phase 3, phantom-mkn.7) — the Swift twins
// of rust/phantom-core/src/plan.rs. Field names map 1:1 to the camelCase
// keys; pinned by tests/fixtures/reclaim-plan.json.
//
// The app uses a plan for exactly one thing: "Copy as script" on the
// reclaimable pane. It never runs the script — Phantom never deletes — it
// puts the text on the clipboard for the user to inspect and run.

import Foundation

public struct ReclaimPlanItem: Codable, Equatable, Sendable {
    public let ruleId: String
    public let label: String
    public let category: String
    public let riskTier: String
    public let why: String
    /// The owning tool's own clean command, or nil (present-as-null).
    public let command: String?
    public let paths: [String]
    /// The group's privateSize — what removing the paths actually frees.
    public let expectedFreedBytes: UInt64
    public let diskSize: UInt64

    enum CodingKeys: String, CodingKey {
        case ruleId, label, category, riskTier, why, command, paths, expectedFreedBytes, diskSize
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(ruleId, forKey: .ruleId)
        try c.encode(label, forKey: .label)
        try c.encode(category, forKey: .category)
        try c.encode(riskTier, forKey: .riskTier)
        try c.encode(why, forKey: .why)
        if let command { try c.encode(command, forKey: .command) } else { try c.encodeNil(forKey: .command) }
        try c.encode(paths, forKey: .paths)
        try c.encode(expectedFreedBytes, forKey: .expectedFreedBytes)
        try c.encode(diskSize, forKey: .diskSize)
    }
}

public struct PlanSkipped: Codable, Equatable, Sendable {
    public let review: UInt64
    public let aboveTier: UInt64
    public let belowMinBytes: UInt64
    /// Every path sat inside a git work tree and was not ignored by its
    /// rules — git data, never a plan path (1.1.0, phantom-2lz).
    public let tracked: UInt64
}

public struct ReclaimPlan: Codable, Equatable, Sendable, Identifiable {
    public var id: UUID { planId }

    public let planId: UUID
    public let scanId: UUID
    public let rootPath: String
    public let createdAt: Date
    public let maxTier: String
    public let minBytes: UInt64
    public let items: [ReclaimPlanItem]
    public let itemCount: UInt64
    public let expectedFreedBytes: UInt64
    public let skipped: PlanSkipped

    enum CodingKeys: String, CodingKey {
        case planId, scanId, rootPath, createdAt, maxTier, minBytes, items, itemCount, expectedFreedBytes, skipped
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        // UUIDs lowercase out (wire rule 4), like Scan.
        try c.encode(planId.uuidString.lowercased(), forKey: .planId)
        try c.encode(scanId.uuidString.lowercased(), forKey: .scanId)
        try c.encode(rootPath, forKey: .rootPath)
        try c.encode(createdAt, forKey: .createdAt)
        try c.encode(maxTier, forKey: .maxTier)
        try c.encode(minBytes, forKey: .minBytes)
        try c.encode(items, forKey: .items)
        try c.encode(itemCount, forKey: .itemCount)
        try c.encode(expectedFreedBytes, forKey: .expectedFreedBytes)
        try c.encode(skipped, forKey: .skipped)
    }
}
