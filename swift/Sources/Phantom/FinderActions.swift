// The two file affordances Phantom offers, shared by every surface (tree
// rows, file-table rows, treemap tiles, Reclaimable groups): REVEAL IN
// FINDER and COPY PATH. There is deliberately nothing else here — Phantom
// never deletes, so no surface gets a destructive action to generalize.

import AppKit
import Quartz

enum FinderActions {
    static func reveal(_ path: String) {
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    static func copyPath(_ path: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(path, forType: .string)
    }

    static func copyCommand(_ command: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(command, forType: .string)
    }

    /// POSIX single-quote a path so it pastes into a shell verbatim —
    /// spaces, `&`, `$`, backticks, and the rest are inert inside single
    /// quotes; an embedded single quote is closed, escaped, reopened
    /// (`'\''`). Phantom composes runnable strings but never runs them.
    static func shellQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// The safe cleanup command a group suggests, made paste-and-run: cd
    /// into the directory the tool must run from — the project dir, which is
    /// the PARENT of the listed artifact (`cargo clean` lives beside
    /// Cargo.toml, one level up from `target/`; likewise `.build/`) — then
    /// the command. Global tools (`brew cleanup`) don't care about cwd, so
    /// the `cd` is harmless there. Copies; never executes.
    static func runnableCommand(_ command: String, forArtifactAt path: String) -> String {
        let runDir = (path as NSString).deletingLastPathComponent
        return "cd \(shellQuote(runDir)) && \(command)"
    }

    static func copyRunnableCommand(_ command: String, forArtifactAt path: String) {
        copyCommand(runnableCommand(command, forArtifactAt: path))
    }

    /// Deep-link straight to System Settings → Privacy & Security → Full
    /// Disk Access. macOS provides no API to grant it — the pane is the
    /// ceiling for every app in this category.
    static func openFullDiskAccessSettings() {
        guard let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"
        ) else { return }
        NSWorkspace.shared.open(url)
    }
}

/// Quick Look glue: one shared data source feeding QLPreviewPanel from
/// whichever surface pressed Space. Folders preview too — the platform
/// default folder preview is fine.
@MainActor
final class QuickLookController: NSObject, QLPreviewPanelDataSource {
    static let shared = QuickLookController()

    private var url: URL?

    /// Space semantics: same item toggles the panel closed; a different
    /// item retargets an open panel.
    func toggle(path: String) {
        let target = URL(fileURLWithPath: path)
        guard let panel = QLPreviewPanel.shared() else { return }
        if panel.isVisible, url == target {
            panel.orderOut(nil)
            return
        }
        url = target
        panel.dataSource = self
        panel.reloadData()
        panel.makeKeyAndOrderFront(nil)
    }

    nonisolated func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int {
        1
    }

    nonisolated func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> QLPreviewItem! {
        MainActor.assumeIsolated { url as NSURL? }
    }
}
