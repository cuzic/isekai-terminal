import SwiftUI

/// Phase 1D/1F: Android版`ProfileListScreen.kt`/`ProfileListViewModel.kt`のMVP部分の
/// 移植。Phase 1F-3(#50)でアプリ全体の既定配色テーマ選択を追加した。定型文管理は
/// この実装のスコープに含めない(別途後続で対応)。
@MainActor
public final class ProfileListModel: ObservableObject {
    @Published public private(set) var profiles: [ConnectionProfile] = []
    @Published public var passwordTarget: ConnectionProfile?
    @Published public var deleteTarget: ConnectionProfile?

    private let db: ProfileDatabase

    public init(db: ProfileDatabase = AppServices.shared.db) {
        self.db = db
    }

    public func load() {
        profiles = (try? db.fetchAllProfiles()) ?? []
    }

    public func requestPasswordConnect(_ profile: ConnectionProfile) { passwordTarget = profile }
    public func dismissPassword() { passwordTarget = nil }

    public func requestDelete(_ profile: ConnectionProfile) { deleteTarget = profile }
    public func dismissDelete() { deleteTarget = nil }

    public func confirmDelete(_ profile: ConnectionProfile) {
        deleteTarget = nil
        guard let id = profile.id else { return }
        try? db.deleteProfile(id: id)
        load()
    }
}

public struct ProfileListView: View {
    @StateObject private var model: ProfileListModel
    private let onConnect: (ConnectionProfile, String?, String?) -> Void
    private let onAddProfile: () -> Void
    private let onEditProfile: (ConnectionProfile) -> Void
    private let onManageKeys: () -> Void
    private let onManageSnippets: () -> Void
    private let onShowDiagnostics: (() -> Void)?

    /// Phase 1F-3(#50): アプリ全体の既定配色テーマ(Android版`SharedPreferences`の
    /// `PREF_KEY`と同じキーで`UserDefaults`へ永続化、プロファイル単位ではなくグローバル設定)。
    @AppStorage(TerminalThemes.prefKey) private var currentThemeName: String = TerminalThemes.defaultDark.name
    @State private var showThemePicker = false

    // `model`にデフォルト値を持たせると、そのデフォルト式`ProfileListModel()`は
    // (SwiftのStateObject(wrappedValue:)のautoclosureとは違い)呼び出し側の
    // 非isolatedなコンテキストで即座に評価されるため、`@MainActor`な
    // `ProfileListModel.init()`を呼べずコンパイルエラーになる。そのためデフォルト値は
    // 持たせず、呼び出し側(`body`、MainActor)で明示的に構築してもらう。
    public init(
        model: ProfileListModel,
        onConnect: @escaping (ConnectionProfile, String?, String?) -> Void,
        onAddProfile: @escaping () -> Void,
        onEditProfile: @escaping (ConnectionProfile) -> Void,
        onManageKeys: @escaping () -> Void,
        onManageSnippets: @escaping () -> Void = {},
        onShowDiagnostics: (() -> Void)? = nil
    ) {
        _model = StateObject(wrappedValue: model)
        self.onConnect = onConnect
        self.onAddProfile = onAddProfile
        self.onEditProfile = onEditProfile
        self.onManageKeys = onManageKeys
        self.onManageSnippets = onManageSnippets
        self.onShowDiagnostics = onShowDiagnostics
    }

    public var body: some View {
        List {
            if model.profiles.isEmpty {
                Text("「＋」をタップして接続先を追加")
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("profileListEmptyHint")
            }
            ForEach(model.profiles, id: \.id) { profile in
                ProfileRow(profile: profile)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        // Android版ProfileListScreen.ktと同じ判断: 主接続または踏み台の
                        // どちらかがパスワード認証ならプロンプトを出す。
                        let needsPasswordPrompt = profile.keyEntryId == nil
                            || (profile.usesJumpHost && profile.jumpKeyEntryId == nil)
                        if needsPasswordPrompt {
                            model.requestPasswordConnect(profile)
                        } else {
                            onConnect(profile, nil, nil)
                        }
                    }
                    .accessibilityIdentifier("profileRow_\(profile.id.map(String.init) ?? "new")")
                    .swipeActions {
                        Button("削除", role: .destructive) { model.requestDelete(profile) }
                        Button("編集") { onEditProfile(profile) }.tint(.blue)
                    }
            }
        }
        .accessibilityIdentifier("profileList")
        .navigationTitle("接続先")
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Menu {
                    Button("鍵管理", action: onManageKeys)
                        .accessibilityIdentifier("manageKeysMenuItem")
                    Button("配色テーマ") { showThemePicker = true }
                        .accessibilityIdentifier("themePickerMenuItem")
                    Button("定型コマンド", action: onManageSnippets)
                        .accessibilityIdentifier("manageSnippetsMenuItem")
                    if let onShowDiagnostics {
                        Button("診断 (Phase 1A-1)", action: onShowDiagnostics)
                            .accessibilityIdentifier("diagnosticsMenuItem")
                    }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .accessibilityIdentifier("profileListMenu")
            }
            ToolbarItem(placement: .navigationBarTrailing) {
                Button(action: onAddProfile) {
                    Image(systemName: "plus")
                }
                .accessibilityIdentifier("addProfileButton")
            }
        }
        .onAppear { model.load() }
        .alert(
            "削除確認",
            isPresented: Binding(
                get: { model.deleteTarget != nil },
                set: { if !$0 { model.dismissDelete() } }
            )
        ) {
            Button("キャンセル", role: .cancel) { model.dismissDelete() }
            Button("削除", role: .destructive) {
                if let target = model.deleteTarget { model.confirmDelete(target) }
            }
        } message: {
            Text("「\(model.deleteTarget?.displayName ?? "")」を削除しますか？")
        }
        .sheet(
            isPresented: Binding(
                get: { model.passwordTarget != nil },
                set: { if !$0 { model.dismissPassword() } }
            )
        ) {
            if let target = model.passwordTarget {
                PasswordPromptView(
                    label: target.displayName,
                    showMainField: target.keyEntryId == nil,
                    jumpLabel: (target.usesJumpHost && target.jumpKeyEntryId == nil) ? target.jumpHost : nil,
                    onConfirm: { password, jumpPassword in
                        model.dismissPassword()
                        onConnect(target, password, jumpPassword)
                    },
                    onCancel: { model.dismissPassword() }
                )
            }
        }
        .sheet(isPresented: $showThemePicker) {
            TerminalThemePickerView(selectedName: $currentThemeName)
        }
    }
}

/// Phase 1F-3(#50): アプリ全体の既定配色テーマ選択シート。Android版
/// `ProfileListScreen.kt`の`TerminalThemeDialog`と同じ役割。
private struct TerminalThemePickerView: View {
    @Binding var selectedName: String
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List(TerminalThemes.all, id: \.name) { theme in
                Button {
                    selectedName = theme.name
                    dismiss()
                } label: {
                    HStack {
                        Circle()
                            .fill(theme.backgroundColor)
                            .frame(width: 20, height: 20)
                            .overlay(Circle().stroke(Color.secondary, lineWidth: 1))
                        Text(theme.name)
                            .foregroundStyle(.primary)
                        Spacer()
                        if theme.name == selectedName {
                            Image(systemName: "checkmark")
                                .foregroundStyle(.tint)
                        }
                    }
                }
                .accessibilityIdentifier("themeOption_\(theme.name)")
            }
            .navigationTitle("配色テーマ")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("閉じる") { dismiss() }
                }
            }
        }
    }
}

private struct ProfileRow: View {
    let profile: ConnectionProfile

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(profile.displayName)
                .font(.headline)
            Text("\(profile.username)@\(profile.host):\(profile.port)")
                .font(.system(.subheadline, design: .monospaced))
                .foregroundStyle(.secondary)
            Text(profile.keyEntryId == nil ? "パスワード" : "鍵認証")
                .font(.caption)
                .foregroundStyle(.tint)
        }
        .padding(.vertical, 2)
    }
}

/// パスワード入力用のシート。Android版`PasswordDialog`相当。`showMainField`が
/// falseの場合(対象ホスト自体は鍵認証だが踏み台がパスワード認証)は踏み台分の
/// フィールドだけを表示する。
struct PasswordPromptView: View {
    let label: String
    let showMainField: Bool
    let jumpLabel: String?
    let onConfirm: (String, String?) -> Void
    let onCancel: () -> Void

    @State private var password: String = ""
    @State private var jumpPassword: String = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                if showMainField {
                    Section("「\(label)」のパスワード") {
                        SecureField("パスワード", text: $password)
                            .accessibilityIdentifier("passwordField")
                    }
                }
                if let jumpLabel {
                    Section("踏み台「\(jumpLabel)」のパスワード") {
                        SecureField("パスワード", text: $jumpPassword)
                            .accessibilityIdentifier("jumpPasswordField")
                    }
                }
            }
            .navigationTitle("パスワード入力")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("キャンセル") { onCancel(); dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("接続") {
                        onConfirm(password, jumpLabel != nil ? jumpPassword : nil)
                        dismiss()
                    }
                    .accessibilityIdentifier("passwordConfirmButton")
                }
            }
        }
    }
}
