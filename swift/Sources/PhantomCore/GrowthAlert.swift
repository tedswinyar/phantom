// GrowthAlert — the growth notification's brain (v1.2 Phase 2,
// phantom-adq.2), as a pure function so every sentence it can say is
// pinned by a test. The model decides WHEN to evaluate (only scheduled
// completions, once per scan id — Phase 1 supplies `startedBy`); this file
// decides WHETHER there is anything worth saying and WHAT.
//
// Rules, all of them:
// - Only growth speaks. Shrinkage is never a notification.
// - Growth must clear BOTH a byte floor and a percent floor's OR: ≥ minBytes
//   OR ≥ minPercent of the previous total. Defaults are high on purpose
//   (5 GB / 5 %): a notification the user mutes in a week was a bug.
// - The body names the category that grew most (from the category series'
//   last two points) and what is safe to reclaim (the summary's estimate).
// - A forecast that crosses "full in under `forecastDays`" is a second,
//   rarer alert, and its body carries the forecast's caveat verbatim.
// - Everything here is local: no egress, no wire change.

import Foundation

public struct GrowthAlertThresholds: Equatable, Sendable {
    /// Growth below this AND below `minPercent` is silent.
    public var minBytes: UInt64
    /// Percent of the previous total.
    public var minPercent: Double
    /// A forecast under this many days to full is worth a word.
    public var forecastDays: Double

    public init(minBytes: UInt64 = 5_000_000_000, minPercent: Double = 5, forecastDays: Double = 30) {
        self.minBytes = minBytes
        self.minPercent = minPercent
        self.forecastDays = forecastDays
    }

    public static let standard = GrowthAlertThresholds()
}

public struct GrowthAlert: Equatable, Sendable {
    public enum Kind: Equatable, Sendable {
        /// The root grew past the thresholds since the previous scan.
        case grew
        /// The linear forecast says the volume fills within `forecastDays`.
        case forecastFull
    }

    public let kind: Kind
    public let scanID: UUID
    public let rootPath: String
    /// Bytes gained since the previous scan (0 for a forecast alert on a
    /// flat root — cannot happen: a forecast alert needs a positive slope).
    public let grewBytes: UInt64
    /// Percent of the previous total.
    public let grewPercent: Double
    /// Days between the previous scan and this one.
    public let periodDays: Double
    /// The classifier category whose bytes rose most between the last two
    /// points of the category series, if any rose.
    public let topCategory: (key: String, bytes: UInt64)?
    /// The summary's reclaim estimate for this scan, if a summary exists.
    public let reclaimEstimate: UInt64?
    /// For `.forecastFull`: the forecast's days and caveat.
    public let daysUntilFull: Double?
    public let caveat: String?

    public static func == (lhs: GrowthAlert, rhs: GrowthAlert) -> Bool {
        lhs.kind == rhs.kind && lhs.scanID == rhs.scanID && lhs.rootPath == rhs.rootPath
            && lhs.grewBytes == rhs.grewBytes && lhs.grewPercent == rhs.grewPercent
            && lhs.periodDays == rhs.periodDays && lhs.topCategory?.key == rhs.topCategory?.key
            && lhs.topCategory?.bytes == rhs.topCategory?.bytes && lhs.reclaimEstimate == rhs.reclaimEstimate
            && lhs.daysUntilFull == rhs.daysUntilFull && lhs.caveat == rhs.caveat
    }

    // MARK: - Evaluate

    /// `previous` and `current` are complete scans of the same root, older
    /// first. `growth` is the root's series by CATEGORY (may be any groupBy;
    /// only a `category` series yields a topCategory). `hotspots` is the
    /// current scan's summary. Returns nil when there is nothing to say.
    public static func evaluate(
        previous: Scan,
        current: Scan,
        growth: Growth?,
        hotspots: HotspotsSummary?,
        thresholds: GrowthAlertThresholds = .standard
    ) -> GrowthAlert? {
        guard current.totalDiskSize > previous.totalDiskSize else { return nil } // shrinkage and flat are silent
        let grew = current.totalDiskSize - previous.totalDiskSize
        let percent = previous.totalDiskSize == 0 ? 100 : Double(grew) / Double(previous.totalDiskSize) * 100
        let period = current.startedAt.timeIntervalSince(previous.startedAt) / 86_400
        let top = topCategory(in: growth)
        let reclaim = hotspots?.reclaimEstimate

        if grew >= thresholds.minBytes || percent >= thresholds.minPercent {
            return GrowthAlert(
                kind: .grew, scanID: current.id, rootPath: current.rootPath, grewBytes: grew,
                grewPercent: percent, periodDays: period, topCategory: top, reclaimEstimate: reclaim,
                daysUntilFull: nil, caveat: nil
            )
        }
        if let f = growth?.forecast, f.isGrowing, let days = f.daysUntilFull, days < thresholds.forecastDays {
            return GrowthAlert(
                kind: .forecastFull, scanID: current.id, rootPath: current.rootPath, grewBytes: grew,
                grewPercent: percent, periodDays: period, topCategory: top, reclaimEstimate: reclaim,
                daysUntilFull: days, caveat: f.caveat
            )
        }
        return nil
    }

    /// The category whose value rose most between the series' last two
    /// points. Nil without two points, without a `category` breakdown, or
    /// when nothing rose. `other` never wins a headline.
    static func topCategory(in growth: Growth?) -> (key: String, bytes: UInt64)? {
        guard let growth, growth.groupBy == "category", growth.points.count >= 2 else { return nil }
        var best: (key: String, bytes: UInt64)?
        for line in growth.series where line.key != "other" {
            guard line.values.count >= 2,
                  let last = line.values[line.values.count - 1],
                  let prev = line.values[line.values.count - 2],
                  last > prev
            else { continue }
            let rise = last - prev
            if best == nil || rise > best!.bytes || (rise == best!.bytes && line.key < best!.key) {
                best = (line.key, rise)
            }
        }
        return best
    }

    // MARK: - Words

    /// Human name for a classifier category key; unknown keys pass through
    /// so a new server category still reads as something.
    public static func categoryLabel(_ key: String) -> String {
        switch key {
        case "regenerableArtifact": return "build artifacts"
        case "staleProjectArtifact": return "stale project artifacts"
        case "cache": return "caches"
        case "toolManagedCache": return "tool-managed caches"
        case "modelCache": return "model caches"
        case "cloudDataloaded": return "cloud placeholders"
        case "reviewFirst": return "files to review"
        default: return key
        }
    }

    /// "Phantom: ~ grew 20.3 GB this week" / "Phantom: ~ may be full in 12 days".
    /// `size` renders bytes the way the app does (DesignKit's `Format.size`).
    public func title(size: (UInt64) -> String) -> String {
        let root = Self.shortRoot(rootPath)
        switch kind {
        case .grew:
            return "Phantom: \(root) grew \(size(grewBytes)) \(Self.periodPhrase(periodDays))"
        case .forecastFull:
            let days = Int((daysUntilFull ?? 0).rounded())
            return "Phantom: \(root) may be full in \(days) day\(days == 1 ? "" : "s")"
        }
    }

    /// One or two sentences: the category that grew, what is safe to
    /// reclaim, where to look — and for a forecast, its caveat.
    public func body(size: (UInt64) -> String) -> String {
        var parts: [String] = []
        if let top = topCategory {
            parts.append("\(size(top.bytes)) of it \(Self.categoryLabel(top.key))")
        }
        if let reclaim = reclaimEstimate, reclaim > 0 {
            parts.append("safe to reclaim: \(size(reclaim))")
        }
        var text = parts.isEmpty ? "" : parts.joined(separator: " · ") + ". "
        switch kind {
        case .grew:
            text += "Open the History pane."
        case .forecastFull:
            text += "At the recent rate. " + (caveat ?? "")
        }
        return text.trimmingCharacters(in: .whitespaces)
    }

    static func periodPhrase(_ days: Double) -> String {
        switch days {
        case ..<1.5: return "since yesterday"
        case ..<8: return "this week"
        case ..<35: return "in \(Int(days.rounded())) days"
        default: return "since \(Int(days.rounded())) days ago"
        }
    }

    /// `/Users/ted` → `~`; anything else → its last component.
    static func shortRoot(_ path: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        if path == home || path == home + "/" { return "~" }
        let trimmed = path.hasSuffix("/") && path.count > 1 ? String(path.dropLast()) : path
        return (trimmed as NSString).lastPathComponent.isEmpty ? trimmed : (trimmed as NSString).lastPathComponent
    }
}

/// Where a notification goes. The app hands `UNUserNotificationCenter`;
/// tests hand a recorder. Kept as a protocol so the model never imports
/// UserNotifications.
public protocol GrowthAlertDelivering: Sendable {
    func deliver(title: String, body: String, scanID: UUID) async
}

/// Which scans have already produced a notification — ever. Persisted so
/// a relaunch does not re-notify; the model consults it before evaluating.
public protocol NotifiedScanStore: Sendable {
    func contains(_ id: UUID) -> Bool
    func insert(_ id: UUID)
}

/// UserDefaults-backed store; the key holds an array of lowercase UUID
/// strings, capped so it cannot grow forever (a scan id that fell off the
/// cap is far older than any retained scan).
public final class DefaultsNotifiedScanStore: NotifiedScanStore, @unchecked Sendable {
    public static let key = "growthAlertNotifiedScanIDs"
    public static let cap = 500
    private let defaults: UserDefaults
    private let lock = NSLock()

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func contains(_ id: UUID) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return (defaults.stringArray(forKey: Self.key) ?? []).contains(id.uuidString.lowercased())
    }

    public func insert(_ id: UUID) {
        lock.lock(); defer { lock.unlock() }
        var ids = defaults.stringArray(forKey: Self.key) ?? []
        let s = id.uuidString.lowercased()
        guard !ids.contains(s) else { return }
        ids.append(s)
        if ids.count > Self.cap { ids.removeFirst(ids.count - Self.cap) }
        defaults.set(ids, forKey: Self.key)
    }
}
