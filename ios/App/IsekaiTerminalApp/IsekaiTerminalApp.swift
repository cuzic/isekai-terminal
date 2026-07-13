import SwiftUI
import IsekaiTerminalCore

@main
struct IsekaiTerminalApp: App {
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
    /// Phase 1G-2(#54): 複数タブ/複数セッションのホスト画面。旧`.terminal(profile:...)`
    /// (1画面1セッション)から置き換えた。状態は関連値ではなく`AppRootView`が保持する
    /// `TerminalTabsModel`側にあるため、associated valueを持たない
    /// (`AppRootView`が離れて戻ってきても同じ`tabsModel`インスタンスを参照する)。
    case terminalHost
    case diagnostics
    case snippetList
    case snippetEdit(Snippet?)
    case keySequenceList
    case keySequenceEdit(KeySequence?)
}

struct AppRootView: View {
    @State private var path: [AppRoute] = []
    // `@StateObject`のプロパティ宣言に直接デフォルト式を書かず、明示的な`init`内で
    // `StateObject(wrappedValue:)`のautoclosure経由で構築する(`ProfileEditView`等
    // 既存コードと同じ回避策 — `@MainActor`な型のデフォルト式が呼び出し側の
    // 非isolatedなコンテキストで評価されコンパイルエラーになる問題を避けるため)。
    @StateObject private var tabsModel: TerminalTabsModel

    init() {
        _tabsModel = StateObject(wrappedValue: TerminalTabsModel())
        // Android版`MainActivity.onCreate`の`restorePersistedCtlSocketForward()`に相当
        // (前回設定した「tmux迂回control-plane」をRust側のプロセスグローバル状態へ
        // 起動直後に一度反映する)。
        CtlSocketForwardSettings.restore()
    }

    var body: some View {
        NavigationStack(path: $path) {
            ProfileListView(
                model: ProfileListModel(),
                onConnect: { profile, password, jumpPassword in
                    tabsModel.openTab(profile: profile, password: password, jumpPassword: jumpPassword)
                    if !path.contains(.terminalHost) {
                        path.append(.terminalHost)
                    }
                },
                onAddProfile: { path.append(.profileEdit(nil)) },
                onEditProfile: { profile in path.append(.profileEdit(profile)) },
                onManageKeys: { path.append(.keyList) },
                onManageSnippets: { path.append(.snippetList) },
                onManageKeySequences: { path.append(.keySequenceList) },
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
                case .terminalHost:
                    TerminalTabsHostView(
                        tabsModel: tabsModel,
                        onAllTabsClosed: { path.removeAll { $0 == .terminalHost } }
                    )
                case .diagnostics:
                    ContentView()
                case .snippetList:
                    SnippetListView(
                        model: SnippetListModel(),
                        onAddSnippet: { path.append(.snippetEdit(nil)) },
                        onEditSnippet: { snippet in path.append(.snippetEdit(snippet)) }
                    )
                case .snippetEdit(let snippet):
                    SnippetEditView(
                        snippet: snippet,
                        onSave: { path.removeLast() },
                        onCancel: { path.removeLast() }
                    )
                case .keySequenceList:
                    KeySequenceListView(
                        model: KeySequenceListModel(),
                        onAddKeySequence: { path.append(.keySequenceEdit(nil)) },
                        onEditKeySequence: { keySequence in path.append(.keySequenceEdit(keySequence)) }
                    )
                case .keySequenceEdit(let keySequence):
                    KeySequenceEditView(
                        keySequence: keySequence,
                        onSave: { path.removeLast() },
                        onCancel: { path.removeLast() }
                    )
                }
            }
        }
        // Android版`applyScreenProtection`(`FLAG_SECURE`)相当。「最近使ったアプリ」の
        // サムネイルへ実内容が写り込むのを防ぐ(`ScreenProtectionOverlay`参照)。
        .screenProtected()
    }
}
