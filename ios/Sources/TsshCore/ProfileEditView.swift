import SwiftUI

/// Phase 1D/1E: Android版`ProfileEditScreen.kt`相当。label/host/port/username/
/// 認証方式に加え、Phase 1Eで踏み台(ProxyJump)・ポートフォワード・SSH agent転送・
/// 接続方式(プレーンSSH/isekai-helper経由QUIC/自動フォールバック/STUN+SSHランデブーP2P/
/// MASQUE relay P2P)を追加した。マルチパスの選択肢と専用の設定項目は、Rust側は
/// 既に対応済みだがiOS側のセッション接続ロジックがまだ無いため
/// (`TerminalSessionController.connect()`参照)、後続タスク(#46〜#47)で追加する。
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

    // Phase 1E-2: 踏み台(ProxyJump)。
    @Published public var useJumpHost: Bool
    @Published public var jumpHost: String
    @Published public var jumpPort: String
    @Published public var jumpUsername: String
    @Published public var jumpUseKeyAuth: Bool
    @Published public var jumpSelectedKeyEntryId: String?

    // Phase 1E-3: ポートフォワード。
    @Published public var forwards: [StoredPortForward]
    @Published public var allowNonLoopbackForwardBind: Bool

    // Phase 1E-4: SSH agent forwarding。
    @Published public var enableAgentForward: Bool

    // Phase 1A-9/1E-5/1E-6: 接続方式。現時点でiOS側が実際に接続できるのは
    // plainSsh/isekaiHelperQuic/auto/isekaiStunP2pQuic/isekaiLinkRelayQuicの5方式のみ
    // (残りは#46〜#47で追加予定)なので、Pickerの選択肢もこの5つに絞る。
    @Published public var transportPreference: StoredTransportPreference
    // Phase 1E-5: STUN+SSHランデブーP2P選択時のみ使うSTUNサーバー(host:port)。
    // 空ならAndroid版と同じ既定値(`defaultStunServer`)にフォールバックする。
    @Published public var stunServer: String

    // Phase 1E-6: MASQUE relay P2P選択時のみ使う。relayJwtはUI上は平文で編集するが、
    // 保存時に`relayVault`で暗号化してDBへ書き込む(Android版`encryptRelayJwt`/
    // `decryptRelayJwt`と同じ方針)。
    @Published public var relayAddr: String
    @Published public var relaySni: String
    @Published public var relayJwt: String

    private let db: ProfileDatabase
    private let relayVault: RelayCredentialVault
    private let existingId: Int64?
    private let existingCreatedAt: Date

    public init(profile: ConnectionProfile?, db: ProfileDatabase = AppServices.shared.db, relayVault: RelayCredentialVault = AppServices.shared.relayVault) {
        self.db = db
        self.relayVault = relayVault
        self.existingId = profile?.id
        self.existingCreatedAt = profile?.createdAt ?? Date()
        self.displayName = profile?.displayName ?? ""
        self.host = profile?.host ?? ""
        self.port = profile.map { String($0.port) } ?? "22"
        self.username = profile?.username ?? ""
        self.useKeyAuth = profile?.keyEntryId != nil
        self.selectedKeyEntryId = profile?.keyEntryId

        self.useJumpHost = profile?.usesJumpHost ?? false
        self.jumpHost = profile?.jumpHost ?? ""
        self.jumpPort = profile.map { String($0.jumpPort) } ?? "22"
        self.jumpUsername = profile?.jumpUsername ?? ""
        self.jumpUseKeyAuth = profile?.jumpKeyEntryId != nil
        self.jumpSelectedKeyEntryId = profile?.jumpKeyEntryId

        self.forwards = profile?.forwards ?? []
        self.allowNonLoopbackForwardBind = profile?.allowNonLoopbackForwardBind ?? false

        self.enableAgentForward = profile?.enableAgentForward ?? false

        self.transportPreference = profile?.transportPreference ?? .plainSsh
        self.stunServer = profile?.stunServer ?? ""

        self.relayAddr = profile?.relayAddr ?? ""
        self.relaySni = profile?.relaySni ?? ""
        self.relayJwt = profile?.relayJwt.flatMap { try? relayVault.decrypt($0) } ?? ""
    }

    public func loadAvailableKeys() {
        availableKeys = (try? db.fetchAllKeyEntries()) ?? []
        if useKeyAuth && selectedKeyEntryId == nil {
            selectedKeyEntryId = availableKeys.first?.id
        }
        if jumpUseKeyAuth && jumpSelectedKeyEntryId == nil {
            jumpSelectedKeyEntryId = availableKeys.first?.id
        }
    }

    public func addForward(_ forward: StoredPortForward) {
        forwards.append(forward)
    }

    public func removeForward(at offsets: IndexSet) {
        forwards.remove(atOffsets: offsets)
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

        var resolvedJumpHost: String?
        var resolvedJumpPort = 22
        var resolvedJumpUsername: String?
        var resolvedJumpKeyEntryId: String?
        if useJumpHost {
            guard !jumpHost.trimmingCharacters(in: .whitespaces).isEmpty else {
                errorMessage = "踏み台のホストを入力してください"
                return false
            }
            guard let jumpPortNumber = Int(jumpPort), (1...65535).contains(jumpPortNumber) else {
                errorMessage = "踏み台のポート番号が不正です"
                return false
            }
            guard !jumpUsername.trimmingCharacters(in: .whitespaces).isEmpty else {
                errorMessage = "踏み台のユーザー名を入力してください"
                return false
            }
            if jumpUseKeyAuth && jumpSelectedKeyEntryId == nil {
                errorMessage = "踏み台の鍵を選択してください"
                return false
            }
            resolvedJumpHost = jumpHost
            resolvedJumpPort = jumpPortNumber
            resolvedJumpUsername = jumpUsername
            resolvedJumpKeyEntryId = jumpUseKeyAuth ? jumpSelectedKeyEntryId : nil
        }

        var resolvedRelayJwt: String?
        if transportPreference == .isekaiLinkRelayQuic {
            guard !relayAddr.trimmingCharacters(in: .whitespaces).isEmpty,
                  !relaySni.trimmingCharacters(in: .whitespaces).isEmpty,
                  !relayJwt.trimmingCharacters(in: .whitespaces).isEmpty else {
                errorMessage = "relayアドレス/SNI/JWTを全て入力してください"
                return false
            }
            do {
                resolvedRelayJwt = try relayVault.encrypt(relayJwt)
            } catch {
                errorMessage = "relay JWTの暗号化に失敗しました: \(error)"
                return false
            }
        }

        var profile = ConnectionProfile(
            id: existingId,
            displayName: displayName,
            host: host,
            port: portNumber,
            username: username,
            keyEntryId: useKeyAuth ? selectedKeyEntryId : nil,
            createdAt: existingCreatedAt,
            enableAgentForward: enableAgentForward,
            transportPreference: transportPreference,
            forwards: forwards,
            jumpHost: resolvedJumpHost,
            jumpPort: resolvedJumpPort,
            jumpUsername: resolvedJumpUsername,
            jumpKeyEntryId: resolvedJumpKeyEntryId,
            stunServer: stunServer.trimmingCharacters(in: .whitespaces).isEmpty ? nil : stunServer,
            relayAddr: relayAddr.trimmingCharacters(in: .whitespaces).isEmpty ? nil : relayAddr,
            relaySni: relaySni.trimmingCharacters(in: .whitespaces).isEmpty ? nil : relaySni,
            relayJwt: resolvedRelayJwt,
            allowNonLoopbackForwardBind: allowNonLoopbackForwardBind
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

    @State private var showAddForwardSheet = false

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
                    keyPicker(selection: $model.selectedKeyEntryId, identifier: "keyEntryPicker")
                }
            }

            Section("接続方式") {
                Picker("接続方式", selection: $model.transportPreference) {
                    Text("プレーンSSH").tag(StoredTransportPreference.plainSsh)
                    Text("isekai-helper経由QUIC").tag(StoredTransportPreference.isekaiHelperQuic)
                    Text("自動(QUIC優先、失敗時SSHへ)").tag(StoredTransportPreference.auto)
                    Text("STUN+SSHランデブーP2P").tag(StoredTransportPreference.isekaiStunP2pQuic)
                    Text("MASQUE relay P2P").tag(StoredTransportPreference.isekaiLinkRelayQuic)
                }
                .accessibilityIdentifier("transportPreferencePicker")

                if model.transportPreference == .isekaiStunP2pQuic {
                    TextField("STUNサーバー(host:port)", text: $model.stunServer)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("stunServerField")
                    Text("空欄なら既定のパブリックSTUNサーバーを使います。双方が同じサーバーを使う必要はありません。")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                if model.transportPreference == .isekaiLinkRelayQuic {
                    TextField("relayアドレス(host:port)", text: $model.relayAddr)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("relayAddrField")
                    TextField("relay SNI", text: $model.relaySni)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("relaySniField")
                    TextField("relay JWT", text: $model.relayJwt)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("relayJwtField")
                    Text("MASQUE relay(bound-udp-server)経由で常時到達可能なP2P QUIC接続を行います。JWTは端末内で暗号化して保存します。")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Text("マルチパスは今後のアップデートで追加予定です。")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("踏み台(ProxyJump)") {
                Toggle("踏み台を使用", isOn: $model.useJumpHost)
                    .accessibilityIdentifier("useJumpHostToggle")

                if model.useJumpHost {
                    TextField("踏み台のホスト", text: $model.jumpHost)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("jumpHostField")
                    TextField("踏み台のポート", text: $model.jumpPort)
                        .keyboardType(.numberPad)
                        .accessibilityIdentifier("jumpPortField")
                    TextField("踏み台のユーザー名", text: $model.jumpUsername)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("jumpUsernameField")

                    Picker("踏み台の認証方式", selection: $model.jumpUseKeyAuth) {
                        Text("パスワード").tag(false)
                        Text("鍵認証").tag(true)
                    }
                    .pickerStyle(.segmented)
                    .accessibilityIdentifier("jumpAuthTypePicker")

                    if model.jumpUseKeyAuth {
                        keyPicker(selection: $model.jumpSelectedKeyEntryId, identifier: "jumpKeyEntryPicker")
                    }
                }
            }

            Section("ポートフォワード") {
                ForEach(Array(model.forwards.enumerated()), id: \.offset) { _, forward in
                    Text(forwardSummary(forward))
                        .font(.system(.body, design: .monospaced))
                }
                .onDelete(perform: model.removeForward)

                Button("フォワードを追加") { showAddForwardSheet = true }
                    .accessibilityIdentifier("addForwardButton")

                Toggle("非ループバックのbindを許可", isOn: $model.allowNonLoopbackForwardBind)
                    .accessibilityIdentifier("allowNonLoopbackForwardBindToggle")
                Text("同一LAN上の第三者からアクセスされ得るため、必要な場合のみ有効にしてください。")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("SSH Agent転送") {
                Toggle("Agent転送を有効化", isOn: $model.enableAgentForward)
                    .accessibilityIdentifier("enableAgentForwardToggle")
                Text("サーバー側があなたの鍵での署名をこのアプリに要求できるようになります(署名要求ごとに確認が必要)。")
                    .font(.caption)
                    .foregroundStyle(.secondary)
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
        .sheet(isPresented: $showAddForwardSheet) {
            AddPortForwardView { forward in
                model.addForward(forward)
                showAddForwardSheet = false
            } onCancel: {
                showAddForwardSheet = false
            }
        }
    }

    @ViewBuilder
    private func keyPicker(selection: Binding<String?>, identifier: String) -> some View {
        if model.availableKeys.isEmpty {
            Text("鍵が登録されていません。鍵管理画面から追加してください。")
                .font(.caption)
                .foregroundStyle(.secondary)
        } else {
            Picker("鍵", selection: selection) {
                ForEach(model.availableKeys, id: \.id) { key in
                    Text(key.displayName).tag(Optional(key.id))
                }
            }
            .accessibilityIdentifier(identifier)
        }
    }

    private func forwardSummary(_ forward: StoredPortForward) -> String {
        switch forward.kind {
        case .local:
            return "L: \(forward.bindAddress):\(forward.bindPort) → \(forward.remoteHost):\(forward.remotePort)"
        case .remote:
            return "R: \(forward.bindAddress):\(forward.bindPort) → \(forward.remoteHost):\(forward.remotePort)"
        case .dynamic:
            return "D: \(forward.bindAddress):\(forward.bindPort) (SOCKS)"
        }
    }
}

/// ポートフォワードを1件追加するためのシート。
private struct AddPortForwardView: View {
    let onAdd: (StoredPortForward) -> Void
    let onCancel: () -> Void

    @State private var kind: StoredPortForward.Kind = .local
    @State private var bindAddress = "127.0.0.1"
    @State private var bindPort = ""
    @State private var remoteHost = ""
    @State private var remotePort = ""
    @State private var errorMessage: String?

    var body: some View {
        NavigationStack {
            Form {
                Picker("種別", selection: $kind) {
                    Text("Local (-L)").tag(StoredPortForward.Kind.local)
                    Text("Remote (-R)").tag(StoredPortForward.Kind.remote)
                    Text("Dynamic (-D, SOCKS)").tag(StoredPortForward.Kind.dynamic)
                }
                .accessibilityIdentifier("forwardKindPicker")

                TextField("待受アドレス", text: $bindAddress)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier("forwardBindAddressField")
                TextField("待受ポート", text: $bindPort)
                    .keyboardType(.numberPad)
                    .accessibilityIdentifier("forwardBindPortField")

                if kind != .dynamic {
                    TextField("転送先ホスト", text: $remoteHost)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("forwardRemoteHostField")
                    TextField("転送先ポート", text: $remotePort)
                        .keyboardType(.numberPad)
                        .accessibilityIdentifier("forwardRemotePortField")
                }

                if let errorMessage {
                    Text(errorMessage).foregroundStyle(.red)
                }
            }
            .navigationTitle("フォワードを追加")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("キャンセル", action: onCancel)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("追加") { confirm() }
                        .accessibilityIdentifier("confirmAddForwardButton")
                }
            }
        }
    }

    private func confirm() {
        guard let bindPortNumber = UInt16(bindPort) else {
            errorMessage = "待受ポート番号が不正です"
            return
        }
        var remotePortNumber: UInt16 = 0
        if kind != .dynamic {
            guard !remoteHost.trimmingCharacters(in: .whitespaces).isEmpty else {
                errorMessage = "転送先ホストを入力してください"
                return
            }
            guard let parsed = UInt16(remotePort) else {
                errorMessage = "転送先ポート番号が不正です"
                return
            }
            remotePortNumber = parsed
        }
        onAdd(StoredPortForward(
            kind: kind,
            bindAddress: bindAddress.isEmpty ? "127.0.0.1" : bindAddress,
            bindPort: bindPortNumber,
            remoteHost: remoteHost,
            remotePort: remotePortNumber
        ))
    }
}
