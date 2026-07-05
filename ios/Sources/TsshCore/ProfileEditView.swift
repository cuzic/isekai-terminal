import SwiftUI

/// Phase 1D: Android版`ProfileEditScreen.kt`のMVP部分(label/host/port/username/
/// 認証方式password-or-key)の移植。踏み台(ProxyJump)・relay・multipath・
/// ポートフォワード等は現時点のiOS版`ConnectionProfile`スキーマに無いため対象外
/// (Android版はPhase 7〜10で段階的に追加されたもので、iOS版でも同様に後続タスクで追加する)。
@MainActor
public final class ProfileEditModel: ObservableObject {
    @Published public var displayName: String
    @Published public var host: String
    @Published public var port: String
    @Published public var username: String
    @Published public var useKeyAuth: Bool
    @Published public var selectedKeyEntryId: String?
    @Published public var availableKeys: [KeyEntry] = []
    @Published public var errorMessage: String?

    private let db: ProfileDatabase
    private let existingId: Int64?
    private let existingCreatedAt: Date

    public init(profile: ConnectionProfile?, db: ProfileDatabase = AppServices.shared.db) {
        self.db = db
        self.existingId = profile?.id
        self.existingCreatedAt = profile?.createdAt ?? Date()
        self.displayName = profile?.displayName ?? ""
        self.host = profile?.host ?? ""
        self.port = profile.map { String($0.port) } ?? "22"
        self.username = profile?.username ?? ""
        self.useKeyAuth = profile?.keyEntryId != nil
        self.selectedKeyEntryId = profile?.keyEntryId
    }

    public func loadAvailableKeys() {
        availableKeys = (try? db.fetchAllKeyEntries()) ?? []
        if useKeyAuth && selectedKeyEntryId == nil {
            selectedKeyEntryId = availableKeys.first?.id
        }
    }

    /// 保存に成功すれば`true`を返す。
    public func save() -> Bool {
        errorMessage = nil
        guard !displayName.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "ラベルを入力してください"
            return false
        }
        guard !host.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "ホストを入力してください"
            return false
        }
        guard let portNumber = Int(port), (1...65535).contains(portNumber) else {
            errorMessage = "ポート番号が不正です"
            return false
        }
        guard !username.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "ユーザー名を入力してください"
            return false
        }
        if useKeyAuth && selectedKeyEntryId == nil {
            errorMessage = "鍵を選択してください"
            return false
        }

        var profile = ConnectionProfile(
            id: existingId,
            displayName: displayName,
            host: host,
            port: portNumber,
            username: username,
            keyEntryId: useKeyAuth ? selectedKeyEntryId : nil,
            createdAt: existingCreatedAt
        )
        do {
            if existingId != nil {
                try db.update(profile: profile)
            } else {
                try db.insert(profile: &profile)
            }
            return true
        } catch {
            errorMessage = "保存に失敗しました: \(error)"
            return false
        }
    }
}

public struct ProfileEditView: View {
    @StateObject private var model: ProfileEditModel
    private let onSave: () -> Void
    private let onCancel: () -> Void

    public init(
        profile: ConnectionProfile?,
        onSave: @escaping () -> Void,
        onCancel: @escaping () -> Void
    ) {
        _model = StateObject(wrappedValue: ProfileEditModel(profile: profile))
        self.onSave = onSave
        self.onCancel = onCancel
    }

    public var body: some View {
        Form {
            Section("接続先") {
                TextField("ラベル", text: $model.displayName)
                    .accessibilityIdentifier("profileLabelField")
                TextField("ホスト", text: $model.host)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier("profileHostField")
                TextField("ポート", text: $model.port)
                    .keyboardType(.numberPad)
                    .accessibilityIdentifier("profilePortField")
                TextField("ユーザー名", text: $model.username)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier("profileUsernameField")
            }

            Section("認証方式") {
                Picker("認証方式", selection: $model.useKeyAuth) {
                    Text("パスワード").tag(false)
                    Text("鍵認証").tag(true)
                }
                .pickerStyle(.segmented)
                .accessibilityIdentifier("authTypePicker")

                if model.useKeyAuth {
                    if model.availableKeys.isEmpty {
                        Text("鍵が登録されていません。鍵管理画面から追加してください。")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        Picker("鍵", selection: $model.selectedKeyEntryId) {
                            ForEach(model.availableKeys, id: \.id) { key in
                                Text(key.displayName).tag(Optional(key.id))
                            }
                        }
                        .accessibilityIdentifier("keyEntryPicker")
                    }
                }
            }

            if let error = model.errorMessage {
                Section {
                    Text(error)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("profileEditError")
                }
            }
        }
        .navigationTitle(model.displayName.isEmpty ? "新規接続先" : model.displayName)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("キャンセル", action: onCancel)
            }
            ToolbarItem(placement: .confirmationAction) {
                Button("保存") {
                    if model.save() { onSave() }
                }
                .accessibilityIdentifier("saveProfileButton")
            }
        }
        .onAppear { model.loadAvailableKeys() }
    }
}
