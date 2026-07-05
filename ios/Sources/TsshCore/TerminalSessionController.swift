import Foundation

/// SSH agentへの署名要求。ユーザーが承認/拒否した結果を`respond`で
/// `TerminalSessionController`(呼び出し元のRustスレッド、`DispatchSemaphore`で
/// 待機中)へ伝える。
public struct AgentSignRequest: Sendable {
    public let id = UUID()
    public let fingerprint: String
    let respond: @Sendable (Bool) -> Void
}

/// Phase 1D: ターミナル本画面のSSH接続状態。UI(`TerminalView`)はこれだけを見て
/// 表示を切り替える(Rust SSOT原則: 接続状態の判断はここに集約し、UI側で
/// 独自のミラー状態を作らない)。
@MainActor
public final class TerminalUIState: ObservableObject {
    public enum State: Equatable {
        case connecting
        case connected
        case disconnected(reason: String?)
        case failed(message: String)
    }

    @Published public internal(set) var state: State = .connecting
    @Published public internal(set) var latestScreenUpdate: ScreenUpdate?
    /// Phase 1E-4: SSH agentへの署名要求。非nilの間、確認ダイアログを表示する。
    @Published public internal(set) var pendingAgentSignRequest: AgentSignRequest?

    // `TerminalSessionController`(非isolated)のstored property初期値として
    // 構築されるため、`nonisolated`にして呼び出し側のコンテキストを問わず
    // 構築できるようにする(`ProfileListView`等のデフォルト引数で踏んだのと
    // 同種のactor分離エラーを、今度はstored propertyの初期化式で踏んだもの)。
    public nonisolated init() {}
}

/// `onAgentSignRequest`(Rustスレッドから同期的にBoolを返す必要がある)の結果を
/// `DispatchSemaphore`越しに受け渡すための小さな箱。`@unchecked Sendable`は、
/// semaphoreのwait/signalが確立するhappens-before関係により`approved`への
/// アクセスが実質的に直列化されることを前提にしている。
private final class AgentSignResultBox: @unchecked Sendable {
    var approved = false
}

/// Phase 1D: `ConnectionProfile`からSSH接続を開始し、`SessionCallback`を実装して
/// Rust側からのイベントを`TerminalUIState`へ橋渡しする。
///
/// `SessionCallback`のメソッドはRustのtokioワーカースレッドから直接呼ばれるため、
/// このクラス自体は`@MainActor`にせず(`onHostKey`/`onAgentSignRequest`が同期的に
/// Boolを返す必要があり、MainActorへのTask hopでは間に合わないため)、UIへ反映する
/// `@Published`な状態は別クラス`TerminalUIState`(`@MainActor`)に分離し、
/// `Task { @MainActor in }`で明示的に受け渡す。
public final class TerminalSessionController: SessionCallback, @unchecked Sendable {
    public let uiState = TerminalUIState()

    private let profile: ConnectionProfile
    private let password: String?
    private let jumpPassword: String?
    private let db: ProfileDatabase
    private let vault: CredentialVault
    private let trustStore: SshHostTrustStore
    private var session: SshSession?

    public init(
        profile: ConnectionProfile,
        password: String?,
        jumpPassword: String? = nil,
        db: ProfileDatabase = AppServices.shared.db,
        vault: CredentialVault = AppServices.shared.vault,
        trustStore: SshHostTrustStore
    ) {
        self.profile = profile
        self.password = password
        self.jumpPassword = jumpPassword
        self.db = db
        self.vault = vault
        self.trustStore = trustStore
    }

    /// 接続を開始する。鍵認証の場合はCredentialVaultから秘密鍵を復号して使う。
    public func connect(cols: UInt32 = 80, rows: UInt32 = 24) {
        guard let auth = resolveAuth(keyEntryId: profile.keyEntryId, password: password, label: "接続先") else {
            return
        }

        var jump: JumpConfig?
        if profile.usesJumpHost {
            guard let jumpHost = profile.jumpHost else {
                fail(message: "踏み台のホストが設定されていません")
                return
            }
            guard let jumpAuth = resolveAuth(keyEntryId: profile.jumpKeyEntryId, password: jumpPassword, label: "踏み台") else {
                return
            }
            jump = JumpConfig(
                host: jumpHost,
                port: UInt16(profile.jumpPort),
                username: profile.jumpUsername ?? "",
                auth: jumpAuth
            )
        }

        // Android版と同じ方針: agent forwardingは公開鍵認証の場合のみ有効にする
        // (パスワード認証には転送すべき鍵材料が無いため)。
        let agentForward = profile.enableAgentForward && profile.keyEntryId != nil

        let config = SshConfig(
            host: profile.host,
            port: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            forwards: profile.forwards.map { $0.asPortForward },
            agentForward: agentForward,
            jump: jump,
            allowNonLoopbackForwardBind: profile.allowNonLoopbackForwardBind
        )

        let newSession = createSshSession(config: config)
        session = newSession
        do {
            try newSession.connect(callback: self)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// `keyEntryId`があればCredentialVaultから秘密鍵を復号して`.publicKey`認証を、
    /// 無ければ渡された`password`で`.password`認証を組み立てる。
    /// 失敗時は`fail(message:)`を呼びnilを返す。
    private func resolveAuth(keyEntryId: String?, password: String?, label: String) -> SshAuth? {
        guard let keyEntryId else {
            return .password(password: password ?? "")
        }
        guard let keyEntry = try? db.fetchKeyEntry(id: keyEntryId) else {
            fail(message: "\(label)の鍵情報が見つかりません")
            return nil
        }
        let metadata = CredentialVault.Metadata(keyId: keyEntry.id, keyType: keyEntry.keyType, publicKey: keyEntry.publicKey)
        guard let pemBytes = try? vault.retrieve(metadata: metadata) else {
            fail(message: "\(label)の秘密鍵の復号に失敗しました")
            return nil
        }
        return .publicKey(privateKeyPem: pemBytes)
    }

    public func send(_ data: Data) {
        session?.send(data: data)
    }

    public func resize(cols: UInt32, rows: UInt32) {
        session?.resize(cols: cols, rows: rows)
    }

    public func disconnect() {
        session?.disconnect()
    }

    /// 保留中のagent署名要求に応答する(UI、MainActorから呼ぶ)。
    @MainActor
    public func respondToAgentSignRequest(approved: Bool) {
        uiState.pendingAgentSignRequest?.respond(approved)
        uiState.pendingAgentSignRequest = nil
    }

    private func fail(message: String) {
        Task { @MainActor in self.uiState.state = .failed(message: message) }
    }

    // MARK: - SessionCallback

    public func onConnected() {
        Task { @MainActor in self.uiState.state = .connected }
    }

    public func onDisconnected(reason: String?) {
        Task { @MainActor in self.uiState.state = .disconnected(reason: reason) }
    }

    public func onScreenUpdate(update: ScreenUpdate) {
        Task { @MainActor in self.uiState.latestScreenUpdate = update }
    }

    /// ホスト鍵確認。iOS版は暫定的にTOFU(Trust On First Use)方式を採る
    /// (Android版`TerminalSession.kt`の`onHostKey`と同じ方針): 初回接続は
    /// 自動的に信頼して記録し、fingerprintが変化した場合のみ拒否する。
    /// `SshHostTrustStore`自体は対話的な確認UIを前提にした設計コメントが
    /// 付いているが、このcallbackはRustスレッドから同期的にBoolを返す必要があり
    /// (接続処理をブロックしてまでUI確認を待つ設計は複雑さに見合わないため)、
    /// 最初の実装ではAndroidと同じ自動信頼方式を踏襲する。対話的な確認UIへの
    /// 格上げは将来の改善候補(PLAN.md参照)。
    public func onHostKey(fingerprint: String) -> Bool {
        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: profile.host, port: UInt16(profile.port))
        switch trustStore.verify(identifier: identifier, keyType: "ssh", fingerprint: fingerprint) {
        case .trustedMatch:
            return true
        case .unknownHost:
            try? trustStore.trust(identifier: identifier, keyType: "ssh", fingerprint: fingerprint)
            return true
        case .mismatch:
            fail(message: "ホスト鍵が変更されています(なりすましの可能性)。接続を中止しました。")
            return false
        }
    }

    public func onData(data: Data) {}
    public func onTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: UInt64?) {}
    public func onTrzszDownloadChunk(transferId: String, data: Data, isLast: Bool) {}
    public func onTrzszProgress(transferId: String, transferred: UInt64, total: UInt64?) {}
    public func onTrzszFinished(transferId: String, success: Bool, message: String?) {}
    public func onNoViablePath() {}
    public func onForwardStateChanged(id: String, state: ForwardState) {}

    /// SSH agentへの署名要求。Android版`AgentSignConfirmDialog`と同じく、要求ごとに
    /// ユーザー確認を必須とする。このcallbackはRustスレッドから同期的にBoolを
    /// 返す必要があるため、`DispatchSemaphore`でMainActor側のダイアログ応答を待つ
    /// (30秒でタイムアウトし拒否扱い、Android版のタイムアウトと同じ方針)。
    public func onAgentSignRequest(keyFingerprint: String) -> Bool {
        let semaphore = DispatchSemaphore(value: 0)
        let resultBox = AgentSignResultBox()
        let request = AgentSignRequest(fingerprint: keyFingerprint) { approved in
            resultBox.approved = approved
            semaphore.signal()
        }

        Task { @MainActor in
            self.uiState.pendingAgentSignRequest = request
        }

        let waitResult = semaphore.wait(timeout: .now() + 30)

        Task { @MainActor in
            if self.uiState.pendingAgentSignRequest?.id == request.id {
                self.uiState.pendingAgentSignRequest = nil
            }
        }

        return waitResult == .success && resultBox.approved
    }
}
