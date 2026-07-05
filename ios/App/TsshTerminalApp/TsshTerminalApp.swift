import SwiftUI
import TsshCore

@main
struct TsshTerminalApp: App {
    var body: some Scene {
        WindowGroup {
            AppRootView()
        }
    }
}

/// Phase 1D: `ProfileListView`を起点としたナビゲーションシェル。
/// Phase 1A-1の診断画面(`ContentView`)はメニューから引き続き到達可能にしておく。
private enum AppRoute: Hashable {
    case profileEdit(ConnectionProfile?)
    case keyList
    case keyImport
    case terminal(ConnectionProfile)
    case diagnostics
}

struct AppRootView: View {
    @State private var path: [AppRoute] = []

    var body: some View {
        NavigationStack(path: $path) {
            ProfileListView(
                model: ProfileListModel(),
                onConnect: { profile, _ in
                    path.append(.terminal(profile))
                },
                onAddProfile: { path.append(.profileEdit(nil)) },
                onEditProfile: { profile in path.append(.profileEdit(profile)) },
                onManageKeys: { path.append(.keyList) },
                onShowDiagnostics: { path.append(.diagnostics) }
            )
            .navigationDestination(for: AppRoute.self) { route in
                switch route {
                case .profileEdit(let profile):
                    ProfileEditView(
                        profile: profile,
                        onSave: { path.removeLast() },
                        onCancel: { path.removeLast() }
                    )
                case .keyList:
                    KeyListView(model: KeyListModel(), onImportKey: { path.append(.keyImport) })
                case .keyImport:
                    KeyImportView(
                        model: KeyImportModel(),
                        onSave: { path.removeLast() },
                        onCancel: { path.removeLast() }
                    )
                case .terminal(let profile):
                    TerminalPlaceholderView(profile: profile)
                case .diagnostics:
                    ContentView()
                }
            }
        }
    }
}
