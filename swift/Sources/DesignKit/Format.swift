// Presentation formatting for the one number this product is about. Views
// format sizes ONLY through this token so every surface renders a byte count
// the same way (Finder-style .file counting, matching the v0.1 app).

import Foundation

public enum Format {
    /// Human-readable size. The caller passes `displaySize`/`diskSize` —
    /// the headline number everywhere — or a labeled logical size in the
    /// inspector's secondary row.
    public static func size(_ bytes: UInt64) -> String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        return formatter.string(fromByteCount: Int64(clamping: bytes))
    }
}
