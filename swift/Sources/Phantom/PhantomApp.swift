import SwiftUI
import PhantomCore
import DesignKit
import Sparkle

/// The one Sparkle seam (phantom-pxt). Owns the updater's lifecycle;
/// `startingUpdater: true` schedules the background check per Sparkle's
/// consent flow (it ASKS the user before ever checking automatically —
/// SECURITY.md documents this as the app's only outbound connection). The
/// feed URL and public key live in Info.plist (build-app.sh); a dev build
/// run outside a bundle simply has no feed and the manual check reports so.
@MainActor
final class UpdaterModel: ObservableObject {
    let controller = SPUStandardUpdaterController(
        startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil
    )
    /// Published so the menu item enables/disables with updater state
    /// (Sparkle exposes this KVO-style; mirrored once at init and on
    /// demand — the item is also guarded by Sparkle itself at click time).
    var canCheck: Bool { controller.updater.canCheckForUpdates }

    func checkForUpdates() {
        controller.checkForUpdates(nil)
    }
}

@main
struct PhantomApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    // @Observable model held as @State (not @StateObject) and passed down via
    // the environment; children read it with @Environment(ScansModel.self).
    @State private var model = ScansModel()
    @StateObject private var updater = UpdaterModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(model)
                // Wire the brand accent so prominent controls (the Haunt
                // button, selection) actually use Palette.accent instead of
                // the system blue — the token was dead code before (design H4).
                .tint(Palette.accent)
                .task { await model.connect() }
        }
        .commands {
            // Phantom > Check for Updates… — the canonical Sparkle spot,
            // right under About.
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…") {
                    updater.checkForUpdates()
                }
            }
            // File > New Haunt… (Cmd-N): starting another analysis must be
            // reachable where every Mac user looks first for "new".
            CommandGroup(replacing: .newItem) {
                Button("New \(Vocabulary.scan)…") {
                    model.showScanSheet = true
                }
                .keyboardShortcut("n", modifiers: .command)
                .disabled(model.connectionState != .connected)
            }
            // View menu: tab selection and the inspector, with shortcuts —
            // the toolbar keeps only New Haunt and the inspector toggle.
            CommandGroup(after: .toolbar) {
                Button("Folders") { model.lowerTab = .folders }
                    .keyboardShortcut("1", modifiers: .command)
                Button("Largest Files") { model.lowerTab = .largestFiles }
                    .keyboardShortcut("2", modifiers: .command)
                Button(Vocabulary.reclaimable) { model.lowerTab = .reclaimable }
                    .keyboardShortcut("3", modifiers: .command)
                Divider()
                Button(model.showInspector ? "Hide \(Vocabulary.inspector)" : "Show \(Vocabulary.inspector)") {
                    model.showInspector.toggle()
                }
                .keyboardShortcut("i", modifiers: [.command, .option])
                Button("Refresh") {
                    Task { await model.refresh() }
                }
                .keyboardShortcut("r", modifiers: .command)
            }
            // Findable later, without waiting for an error callout: the
            // grant flow lives in Help too.
            CommandGroup(after: .help) {
                Button("Grant Full Disk Access…") {
                    FinderActions.openFullDiskAccessSettings()
                }
            }
            // Finder's Enclosing Folder chord backs the treemap out one
            // drill level; Escape does the same via onExitCommand in the
            // detail view.
            CommandMenu("Go") {
                Button("Enclosing Folder") {
                    Task { await model.drillOut() }
                }
                .keyboardShortcut(.upArrow, modifiers: .command)
                .disabled(model.treemapRoot == nil)
            }
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillTerminate(_ notification: Notification) {
        APIServerManager.shared.stop()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
