import SwiftUI
import UIKit
import TsshCoreLogic

/// Phase 1G-2(#54): 複数タブ/複数セッション対応。Android版`TerminalTabsViewModel`
/// (`TabState`のリスト+`activeTabId`)のSwift移植。
///
/// Android版との重要な違い: Android側は単一のForeground Serviceが複数セッションを
/// 共有し、バックグラウンドでもタブが動き続ける設計だが、iOSにはFGS相当の仕組みが
/// 無い。このタスクは「アプリがフォアグラウンドの間、複数セッションを同時に維持する」
/// ことだけをスコープとし、バックグラウンドでの生存は別タスク(#14)に委ねる
/// (Explore agentでのAndroid調査結果を踏まえた意図的なスコープ限定)。
///
/// Android版はタブ追加専用の「+」を持たず、プロファイル一覧へ戻って再接続することで
/// 新規タブを開く(`tabsVm`がActivity scopeで生き続けるため可能)。iOSの
/// `NavigationStack`は破棄されたdestinationを保持しないため、同じ手段だと
/// タブ一覧画面から一度離れただけで全セッションが切断されてしまう。そのため
/// iOS版はタブバーに明示的な「+」ボタンを持たせ、ターミナルタブ一覧画面
/// (`TerminalTabsHostView`)から離れずに新しいタブを開けるようにした
/// (Android版からの意図的なUX変更、プラットフォームの制約に対する適応)。
@MainActor
public final class TerminalTabsModel: ObservableObject {
    public struct Tab: Identifiable {
        public let id = UUID()
        public let profile: ConnectionProfile
        public let controller: TerminalSessionController
    }

    @Published public private(set) var tabs: [Tab] = []
    @Published public var activeTabId: UUID?

    private let trustStore: SshHostTrustStore
    private let db: ProfileDatabase
    private let vault: CredentialVault
    private let relayVault: RelayCredentialVault
    /// Phase 1C(#14): バックグラウンド遷移中、OSに切断されるまでの猶予をもらうための
    /// トークン。iOSにはAndroidのForeground Serviceに相当する仕組みが無く、
    /// `beginBackgroundTask`は約30秒の猶予しか保証しないため、あくまでベストエフォート
    /// (Rust側のtokioワーカーもサスペンド中は停止するため、この間の実際の生存は
    /// helper側の論理セッション/QUIC idle timeoutに委ねる。`PLAN.md`のPhase 0-5の
    /// 机上調査結論を参照)。
    private var backgroundTaskId: UIBackgroundTaskIdentifier = .invalid

    public init(
        trustStore: SshHostTrustStore = AppServices.shared.trustStore,
        db: ProfileDatabase = AppServices.shared.db,
        vault: CredentialVault = AppServices.shared.vault,
        relayVault: RelayCredentialVault = AppServices.shared.relayVault
    ) {
        self.trustStore = trustStore
        self.db = db
        self.vault = vault
        self.relayVault = relayVault
        observeLifecycleNotifications()
    }

    /// Phase 1C(#14): アプリのバックグラウンド遷移/復帰をそのまま受け取る。Android版
    /// `TerminalTabsViewModel`は同種の判断をRust側の状態を見て行っているわけではなく、
    /// UI表示に閉じないセッション生存の話ではあるが、iOS版では現時点でRust側に
    /// バックグラウンド遷移専用のUniFFIメソッドが無い(#24でSessionSupervisor導入後に
    /// 追加予定)ため、暫定的にSwift側で「フォアグラウンド復帰時に切断済みタブを
    /// 再接続する」までを行う(生セッション状態の分岐はconnect()/reconnect()に閉じており、
    /// ここではOSイベントの中継のみ)。
    private func observeLifecycleNotifications() {
        NotificationCenter.default.addObserver(
            forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main
        ) { [weak self] _ in
            Task { @MainActor in self?.handleDidEnterBackground() }
        }
        NotificationCenter.default.addObserver(
            forName: UIApplication.willEnterForegroundNotification, object: nil, queue: .main
        ) { [weak self] _ in
            Task { @MainActor in self?.handleWillEnterForeground() }
        }
    }

    private func handleDidEnterBackground() {
        endBackgroundTaskIfNeeded()
        backgroundTaskId = UIApplication.shared.beginBackgroundTask(withName: "isekai-terminal-sessions") { [weak self] in
            Task { @MainActor in self?.endBackgroundTaskIfNeeded() }
        }
    }

    private func handleWillEnterForeground() {
        endBackgroundTaskIfNeeded()
        for tab in tabs {
            tab.controller.reconnect()
        }
    }

    private func endBackgroundTaskIfNeeded() {
        guard backgroundTaskId != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTaskId)
        backgroundTaskId = .invalid
    }

    /// 新しいタブを開いて接続を開始し、そのタブをアクティブにする。生成したtab idを返す。
    /// Android版`TerminalTabsViewModel.openTab`と同じく、接続はここで即座に開始する
    /// (Viewのマウントタイミングに依存しない)。
    @discardableResult
    public func openTab(profile: ConnectionProfile, password: String?, jumpPassword: String? = nil) -> UUID {
        let controller = TerminalSessionController(
            profile: profile, password: password, jumpPassword: jumpPassword,
            db: db, vault: vault, relayVault: relayVault, trustStore: trustStore
        )
        let tab = Tab(profile: profile, controller: controller)
        tabs.append(tab)
        activeTabId = tab.id
        controller.connect()
        return tab.id
    }

    public func setActiveTab(_ id: UUID) {
        guard tabs.contains(where: { $0.id == id }) else { return }
        activeTabId = id
    }

    /// タブを閉じる。切断してから一覧から除去し、閉じたタブがアクティブだった場合は
    /// 残りのタブのうち最後に開いたものをアクティブにする(Android版`closeTab`と対称)。
    public func closeTab(_ id: UUID) {
        guard let index = tabs.firstIndex(where: { $0.id == id }) else { return }
        tabs[index].controller.disconnect()
        tabs.remove(at: index)
        if activeTabId == id {
            activeTabId = tabs.last?.id
        }
    }
}

/// タブバー+アクティブなタブのターミナル画面をまとめたホスト画面。Android版
/// `TerminalHostScreen.kt`と対称。全タブの`TerminalView`を同時にマウントしたまま
/// 非アクティブなものは不透明度0+ヒットテスト無効にする(Android版が全タブの
/// Composableをコンポジションに残したままゼロサイズにするのと同じ狙い:
/// スクロール位置・選択範囲・IME状態を保持したままタブを切り替えられるようにする)。
public struct TerminalTabsHostView: View {
    @ObservedObject var tabsModel: TerminalTabsModel
    let onAllTabsClosed: () -> Void

    @State private var showAddTabSheet = false

    public init(tabsModel: TerminalTabsModel, onAllTabsClosed: @escaping () -> Void) {
        self.tabsModel = tabsModel
        self.onAllTabsClosed = onAllTabsClosed
    }

    public var body: some View {
        VStack(spacing: 0) {
            tabBar
            ZStack {
                ForEach(tabsModel.tabs) { tab in
                    TerminalView(controller: tab.controller, profile: tab.profile, isActive: tab.id == tabsModel.activeTabId)
                        .opacity(tab.id == tabsModel.activeTabId ? 1 : 0)
                        .allowsHitTesting(tab.id == tabsModel.activeTabId)
                }
            }
        }
        .background(Color.black)
        .navigationBarBackButtonHidden(true)
        // iOS 16対応(このパッケージのplatforms指定): 2引数版onChange(of:initial:_:)は
        // iOS 17+限定のため、1引数版(旧API)を使う。
        .onChange(of: tabsModel.tabs.count) { newCount in
            if newCount == 0 { onAllTabsClosed() }
        }
        .sheet(isPresented: $showAddTabSheet) {
            AddTabProfilePicker(onPick: { profile, password, jumpPassword in
                tabsModel.openTab(profile: profile, password: password, jumpPassword: jumpPassword)
                showAddTabSheet = false
            })
        }
    }

    private var tabBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 4) {
                ForEach(tabsModel.tabs) { tab in
                    TabChip(
                        profile: tab.profile,
                        uiState: tab.controller.uiState,
                        isActive: tab.id == tabsModel.activeTabId,
                        onSelect: { tabsModel.setActiveTab(tab.id) },
                        onClose: { tabsModel.closeTab(tab.id) }
                    )
                }
                Button {
                    showAddTabSheet = true
                } label: {
                    Image(systemName: "plus")
                        .padding(8)
                }
                .accessibilityIdentifier("addTabButton")
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
        }
        .background(Color(.secondarySystemBackground))
        .accessibilityIdentifier("terminalTabBar")
    }
}

/// 1タブ分のタブチップ(状態ドット+ラベル+閉じるボタン)。Android版
/// `TerminalHostScreen.kt`のタブ行(状態ドット・ラベル・🎨・×)のうち、状態ドット/
/// ラベル/×に相当する部分(テーマ切替🎨は#54のスコープ外、必要なら`ProfileEditView`
/// 側のプロファイル固有テーマ設定を使う)。
private struct TabChip: View {
    let profile: ConnectionProfile
    @ObservedObject var uiState: TerminalUIState
    let isActive: Bool
    let onSelect: () -> Void
    let onClose: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            Circle()
                .fill(statusColor)
                .frame(width: 8, height: 8)
            Text(profile.displayName)
                .lineLimit(1)
                .font(.callout)
            Button(action: onClose) {
                Image(systemName: "xmark")
                    .font(.caption2)
            }
            .accessibilityIdentifier("closeTabButton_\(profile.displayName)")
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(isActive ? Color.accentColor.opacity(0.25) : Color.clear)
        .clipShape(Capsule())
        .contentShape(Capsule())
        .onTapGesture(perform: onSelect)
        .accessibilityIdentifier("tabChip_\(profile.displayName)")
    }

    private var statusColor: Color {
        switch uiState.state {
        case .connected: return .green
        case .connecting: return .yellow
        case .disconnected, .failed: return .gray
        }
    }
}

/// タブ追加用のプロファイル選択シート。Android版が「プロファイル一覧へ戻って
/// 再接続する」ことで新規タブを開くのに対し、iOS版はタブ一覧画面から離れずに
/// 開けるようこのシートを使う(このファイル冒頭のコメント参照)。
private struct AddTabProfilePicker: View {
    let onPick: (ConnectionProfile, String?, String?) -> Void

    @StateObject private var model = ProfileListModel()
    @State private var passwordTarget: ConnectionProfile?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                if model.profiles.isEmpty {
                    Text("接続先が登録されていません。")
                        .foregroundStyle(.secondary)
                }
                ForEach(model.profiles, id: \.id) { profile in
                    Button {
                        let needsPasswordPrompt = profile.keyEntryId == nil
                            || (profile.usesJumpHost && profile.jumpKeyEntryId == nil)
                        if needsPasswordPrompt {
                            passwordTarget = profile
                        } else {
                            onPick(profile, nil, nil)
                        }
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(profile.displayName)
                                .font(.headline)
                                .foregroundStyle(.primary)
                            Text("\(profile.username)@\(profile.host):\(profile.port)")
                                .font(.system(.subheadline, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                    }
                    .accessibilityIdentifier("addTabProfileRow_\(profile.id.map(String.init) ?? "new")")
                }
            }
            .navigationTitle("タブを追加")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("キャンセル") { dismiss() }
                }
            }
            .onAppear { model.load() }
            .sheet(
                isPresented: Binding(
                    get: { passwordTarget != nil },
                    set: { if !$0 { passwordTarget = nil } }
                )
            ) {
                if let target = passwordTarget {
                    PasswordPromptView(
                        label: target.displayName,
                        showMainField: target.keyEntryId == nil,
                        jumpLabel: (target.usesJumpHost && target.jumpKeyEntryId == nil) ? target.jumpHost : nil,
                        onConfirm: { password, jumpPassword in
                            passwordTarget = nil
                            onPick(target, password, jumpPassword)
                        },
                        onCancel: { passwordTarget = nil }
                    )
                }
            }
        }
    }
}
