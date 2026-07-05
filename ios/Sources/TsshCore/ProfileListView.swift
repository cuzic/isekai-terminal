import SwiftUI

/// Phase 1D: Android版`ProfileListScreen.kt`/`ProfileListViewModel.kt`のMVP部分の移植。
/// 配色テーマ・定型文管理はこの一次実装のスコープに含めない(別途後続で対応)。
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
    private let onConnect: (ConnectionProfile, String?) -> Void
    private let onAddProfile: () -> Void
    private let onEditProfile: (ConnectionProfile) -> Void
    private let onManageKeys: () -> Void
    private let onShowDiagnostics: (() -> Void)?

    // `model`にデフォルト値を持たせると、そのデフォルト式`ProfileListModel()`は
    // (SwiftのStateObject(wrappedValue:)のautoclosureとは違い)呼び出し側の
    // 非isolatedなコンテキストで即座に評価されるため、`@MainActor`な
    // `ProfileListModel.init()`を呼べずコンパイルエラーになる。そのためデフォルト値は
    // 持たせず、呼び出し側(`body`、MainActor)で明示的に構築してもらう。
    public init(
        model: ProfileListModel,
        onConnect: @escaping (ConnectionProfile, String?) -> Void,
        onAddProfile: @escaping () -> Void,
        onEditProfile: @escaping (ConnectionProfile) -> Void,
        onManageKeys: @escaping () -> Void,
        onShowDiagnostics: (() -> Void)? = nil
    ) {
        _model = StateObject(wrappedValue: model)
        self.onConnect = onConnect
        self.onAddProfile = onAddProfile
        self.onEditProfile = onEditProfile
        self.onManageKeys = onManageKeys
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
                        if profile.keyEntryId == nil {
                            model.requestPasswordConnect(profile)
                        } else {
                            onConnect(profile, nil)
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
                    onConfirm: { password in
                        model.dismissPassword()
                        onConnect(target, password)
                    },
                    onCancel: { model.dismissPassword() }
                )
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

/// パスワード入力用のシンプルなシート。Android版`PasswordDialog`のMVP部分に相当
/// (踏み台のパスワードは、iOS側にまだ踏み台の概念が無いため対象外)。
struct PasswordPromptView: View {
    let label: String
    let onConfirm: (String) -> Void
    let onCancel: () -> Void

    @State private var password: String = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section("「\(label)」のパスワード") {
                    SecureField("パスワード", text: $password)
                        .accessibilityIdentifier("passwordField")
                }
            }
            .navigationTitle("パスワード入力")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("キャンセル") { onCancel(); dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("接続") { onConfirm(password); dismiss() }
                        .accessibilityIdentifier("passwordConfirmButton")
                }
            }
        }
    }
}
