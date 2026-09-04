// Full Disk Access detection. Phantom is non-sandboxed by design, but TCC
// still gates the sensitive corners of the home directory (~/Library/Mail,
// Safari, Messages, …): without the FDA grant a scan of those silently
// under-reports — the walk records permission errors and moves on. The app
// detects the state and GUIDES; it never blocks or nags, because scans that
// need no grant (~/Code) are completely legitimate.
//
// Detection is the house canary pattern: try to LIST a TCC-protected
// directory and classify the failure. macOS offers no API to query the FDA
// grant directly; the probe is the ground truth.
//
// PROCESS MODEL ASSUMPTION: the scanner runs in phantom-api, a child
// process the app spawns. Under TCC's responsible-process semantics the
// child's file access is attributed to the responsible app bundle, so the
// grant made for Phantom.app covers the scanner. The release smoke test
// verifies this end-to-end (scan ~/Library with and without the grant).

import Foundation

/// What one attempt to read a directory found. `denied` and `missing` are
/// DIFFERENT signals: a TCC-blocked canary is evidence about the grant; an
/// absent one says nothing (the user may simply never have run Mail).
public enum FileAccessProbeResult: Equatable, Sendable {
    case readable
    case denied
    case missing
}

/// The one filesystem boundary in FDA detection — mock THIS in tests; the
/// classification logic below it is pure and runs against the mock.
public protocol FileAccessProbing: Sendable {
    func result(forReading path: String) -> FileAccessProbeResult
}

/// The real probe: list the directory, classify the NSError. Cocoa code 257
/// (NSFileReadNoPermission) is the TCC denial; 260 (NSFileReadNoSuchFile)
/// is absence. Anything else is treated as `missing` — an odd error is not
/// evidence about the grant, and FDA claims are only made on evidence.
public struct FileSystemAccessProbe: FileAccessProbing {
    public init() {}

    public func result(forReading path: String) -> FileAccessProbeResult {
        let expanded = (path as NSString).expandingTildeInPath
        do {
            _ = try FileManager.default.contentsOfDirectory(atPath: expanded)
            return .readable
        } catch let error as NSError {
            if error.domain == NSCocoaErrorDomain,
               error.code == NSFileReadNoPermissionError {
                return .denied
            }
            return .missing
        }
    }
}

public enum FullDiskAccess {
    /// What the canaries showed. `undetermined` — no canary existed —
    /// renders NO FDA messaging anywhere: no evidence, no claim.
    public enum Status: Equatable, Sendable {
        case granted
        case notGranted
        case undetermined
    }

    /// TCC-protected directories that exist on virtually every install.
    /// ~/Library/Safari is created by the bundled Safari on first login;
    /// Mail and Messages cover accounts where it somehow is not. Several
    /// canaries, because any ONE of them existing is enough to classify.
    public static let canaryPaths = [
        "~/Library/Safari",
        "~/Library/Mail",
        "~/Library/Messages",
    ]

    /// Classify the grant from the canaries:
    /// - ANY readable canary proves the grant (TCC's Full Disk Access is
    ///   all-or-nothing; one protected dir readable means all are).
    /// - Otherwise any DENIED canary proves its absence.
    /// - All canaries missing: no evidence either way.
    public static func status(
        probe: FileAccessProbing, canaries: [String] = canaryPaths
    ) -> Status {
        var sawDenial = false
        for canary in canaries {
            switch probe.result(forReading: canary) {
            case .readable:
                return .granted
            case .denied:
                sawDenial = true
            case .missing:
                continue
            }
        }
        return sawDenial ? .notGranted : .undetermined
    }
}
