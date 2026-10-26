// Settings (Cmd-,): the standard Mac preferences window — a fixed-width
// TabView of grouped Forms, one tab per concern. Preferences live where the
// system expects them (UserDefaults via Voice; Sparkle's own store for
// update checks), so nothing here is per-scan state and none of it goes
// through ScansModel.

import SwiftUI
import DesignKit
import PhantomCore

struct SettingsView: View {
    @EnvironmentObject private var updater: UpdaterModel

    var body: some View {
        TabView {
            GeneralSettingsTab()
                .tabItem { Label("General", systemImage: "gearshape") }
            UpdatesSettingsTab(updater: updater)
                .tabItem { Label("Updates", systemImage: "arrow.triangle.2.circlepath") }
        }
        // Fixed on both axes: a grouped Form is a scroll view and would
        // otherwise stretch to fill whatever height the window last had.
        .frame(width: Size.settingsWidth, height: Size.settingsHeight)
    }
}

/// General: the product voice. Spooky is OPT-IN — a fresh install reads
/// plainly until this is turned on (Ted, 2026-09-08).
struct GeneralSettingsTab: View {
    var body: some View {
        @Bindable var voice = Vocabulary.voice
        Form {
            Section {
                Toggle(isOn: $voice.spooky) {
                    Text("Use spooky names")
                    Text("Calls a scan a Haunt, a folder a Crypt, the inspector a Séance. Off, Phantom uses plain names.")
                }
            } header: {
                Text("Appearance")
            }
        }
        .formStyle(.grouped)
        .padding(.bottom, Spacing.sm)
    }
}

/// Updates: Sparkle's consent switch and a manual check — the two things
/// every Mac app puts here. Sparkle owns the persisted value; the toggle is
/// a thin binding onto it so this view never keeps a second copy.
struct UpdatesSettingsTab: View {
    let updater: UpdaterModel

    var body: some View {
        Form {
            Section {
                Toggle("Automatically check for updates", isOn: Binding(
                    get: { updater.controller.updater.automaticallyChecksForUpdates },
                    set: { updater.controller.updater.automaticallyChecksForUpdates = $0 }
                ))
                LabeledContent("Version") {
                    Text(Version.marketing)
                        .foregroundStyle(Palette.textSecondary)
                }
                Button("Check for Updates Now…") {
                    updater.checkForUpdates()
                }
                .disabled(!updater.canCheck)
            } header: {
                Text("Software Update")
            }
        }
        .formStyle(.grouped)
        .padding(.bottom, Spacing.sm)
    }
}
