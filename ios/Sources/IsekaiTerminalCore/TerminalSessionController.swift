import Foundation
import Network
import os
import IsekaiTerminalCoreLogic

/// SSH agentへの署名要求。ユーザーが承認/拒否した結果を`respond`で
/// `TerminalSessionController`(呼び出し元のRustスレッド、`DispatchSemaphore`で
/// 待機中)へ伝える。
public struct AgentSignRequest: Sendable {
    public let id = UUID()
    public let fingerprint: String
    let respond: @Sendable (Bool) -> Void
}

/// 初回接続(未知ホスト)の確認待ち。Android版`NewHostKeyPrompt`(`UiState.kt`)と対称。
/// 非nilの間、確認ダイアログを表示する(`TerminalView`の`.alert`参照)。
public struct NewHostKeyPrompt: Equatable, Sendable {
    public let host: String
    public let port: UInt16
    public let fingerprint: String
}

/// Phase 1D: ターミナル本画面のSSH接続状態。UI(`TerminalView`)はこれだけを見て
/// 表示を切り替える(Rust SSOT原則: 接続状態の判断はここに集約し、UI側で
/// 独自のミラー状態を作らない)。
@MainActor
public final class TerminalUIState: ObservableObject {
    public enum State: Equatable {
        case connecting
        case connected
        /// `issueHint`はRust側(`SessionOrchestrator`)が判定した、切断理由の
        /// ヒューリスティックなヒント(`rust-ssot.md`: 判断はRust側、Swiftは
        /// 届いた値に応じて案内UIを出すだけ)。`nil`なら特に案内すべき理由がない。
        case disconnected(reason: String?, issueHint: ConnectionIssueHint? = nil)
        case failed(message: String)
        /// 一度`Connected`になったセッションが予期せず切断され、Rust側の自動
        /// reconnectループが再接続を試みている間の状態(Android版`isReconnecting`
        /// 相当)。`elapsedSecs`/`timeoutSecs`はRust側がライブ通知するSSOT値。
        case reconnecting(elapsedSecs: UInt32, timeoutSecs: UInt32, reason: String?)
    }

    @Published public internal(set) var state: State = .connecting
    @Published public internal(set) var latestScreenUpdate: ScreenUpdate?
    /// Phase 1E-4: SSH agentへの署名要求。非nilの間、確認ダイアログを表示する。
    @Published public internal(set) var pendingAgentSignRequest: AgentSignRequest?
    /// 初回接続(未知ホスト)の確認待ち。非nilの間、確認ダイアログを表示する
    /// (Android版`uiState.newHostKeyPrompt`と対称、`.claude/rules/rust-ssot.md`に
    /// 沿い、未知ホストの自動trustはせずユーザー確認を必須にする)。
    @Published public internal(set) var newHostKeyPrompt: NewHostKeyPrompt?
    /// Phase 1C(#25): trzszファイル転送の状態。非nilの間、転送シートを表示する。
    @Published public internal(set) var trzszState: TrzszUiState?
    /// Phase 1C(#25): ダウンロード完了後、ユーザーがFilesアプリ等へ保存できる
    /// 一時ファイルのURL。`trzszState`が`.done(success: true, ...)`かつダウンロード
    /// だった場合のみ設定される。
    @Published public internal(set) var completedDownloadURL: URL?
    /// Phase 9-6(#16): マルチパスtransportの`RebindManager`状態(WiFi/セルラー
    /// フェイルオーバー/復帰待ち)。マルチパス以外のtransportでは常にnil。表示可否の
    /// 判定は`RebindPublicState`だけを見て行う(Android版`TerminalScreen.kt`と同じ、
    /// rust-ssot.md準拠 — Swift側で独自のミラー状態は持たない)。
    @Published public internal(set) var rebindState: RebindPublicState?

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

/// Android版`ConnectionProfile.DEFAULT_STUN_SERVER`と同じ既定STUNサーバー
/// (双方が同じSTUNサーバーを使う必要は無いため、単なるデフォルト値)。
let defaultStunServer = "stun.l.google.com:19302"

/// Phase 1D: `ConnectionProfile`からSSH接続を開始し、`OrchestratorCallback`を実装して
/// Rust側からのイベントを`TerminalUIState`へ橋渡しする。
///
/// `OrchestratorCallback`のメソッドはRustのtokioワーカースレッドから直接呼ばれるため、
/// このクラス自体は`@MainActor`にせず(`onHostKey`/`onAgentSignRequest`が同期的に
/// Boolを返す必要があり、MainActorへのTask hopでは間に合わないため)、UIへ反映する
/// `@Published`な状態は別クラス`TerminalUIState`(`@MainActor`)に分離し、
/// `Task { @MainActor in }`で明示的に受け渡す。
public final class TerminalSessionController: OrchestratorCallback, @unchecked Sendable {
    public let uiState = TerminalUIState()
    private static let logger = Logger(subsystem: "tools.isekai.terminal", category: "ssh")

    private let profile: ConnectionProfile
    private let password: String?
    private let jumpPassword: String?
    private let db: ProfileDatabase
    private let vault: CredentialVault
    private let relayVault: RelayCredentialVault
    private let trustStore: SshHostTrustStore
    /// 全transport共通の単一セッションオブジェクト(Android版`TerminalSession.kt`と同じ
    /// `SessionOrchestrator`を使う、Phase 1A-9当時の5つの個別transportセッション型からの
    /// 移行)。`init()`の最後で`self`を渡して構築するため IUO にしてある(Swiftの
    /// self参照初期化パターン、`self`が全ストアドプロパティの初期化完了後にしか
    /// 使えない制約を満たすためのもの)。
    private var orchestrator: SessionOrchestrator!
    /// Phase 1C(#14): `reconnect()`が最後に使ったcols/rowsで再接続できるように保持する。
    private var lastCols: UInt32 = 80
    private var lastRows: UInt32 = 24
    /// Phase 1C(#25): 進行中のtrzsz転送のID/mode/表示名。`onTrzszRequest`で設定し、
    /// `trzszDismiss()`でクリアする。Rustスレッド(callback)とUI操作スレッドの両方から
    /// 触るため、単純な代入のみで完結する範囲でしか使わない(複雑な排他制御はしない)。
    private var activeTrzszTransferId: String?
    private var activeTrzszMode: String?
    private var activeTrzszFileName: String?
    /// Phase 1C(#25): ダウンロード完了時に一括で書き込む一時ファイル。`trzszStartDownload()`
    /// で確保し、`onDownloadComplete`が到着したらそこへ書き込む(Rust側が全量を
    /// バッファしてから`onDownloadComplete(fileName:data:)`で一括で渡す設計のため、
    /// 以前のような逐次チャンク書き込みは不要になった)。`trzszStartDownload()`が空の
    /// ファイルを既に作成しているため、0バイトの正常終了(Rust側`orchestrator.rs`の
    /// `on_trzsz_finished`は`data.is_empty()`の場合`onDownloadComplete`自体を呼ばない)
    /// でも有効なファイルとして扱える。
    private var downloadTempURL: URL?
    /// `onDownloadComplete`での書き込みが失敗した場合に`true`。転送完了時、成功扱いでも
    /// `completedDownloadURL`を公開しない(存在しない/不完全なファイルをUIへ渡さない)
    /// ためのガード。
    private var downloadWriteFailed = false
    /// Phase 1C(#26): OSの経路変化を検知するためのmonitor。生イベントをそのまま
    /// `orchestrator.notifyNetworkPathChanged(isSatisfied:)`へ転送するだけで、
    /// debounce/coalesceの判断自体はRust側([`crate::net_health_policy`])に集約されている
    /// (`.claude/rules/rust-ssot.md`)。
    private let networkPathMonitor = NWPathMonitor()
    /// Phase 9-6(#15/#16): `RebindManager`(Rust側)がWiFi/セルラーのfdを要求してきたら
    /// 取得して返すだけの実装(判断はしない、rust-ssot.md準拠)。Android版
    /// `PhysicalPathProvider`のiOS版。
    private let physicalPathProvider = PhysicalPathProvider()

    public init(
        profile: ConnectionProfile,
        password: String?,
        jumpPassword: String? = nil,
        db: ProfileDatabase = AppServices.shared.db,
        vault: CredentialVault = AppServices.shared.vault,
        relayVault: RelayCredentialVault = AppServices.shared.relayVault,
        trustStore: SshHostTrustStore
    ) {
        self.profile = profile
        self.password = password
        self.jumpPassword = jumpPassword
        self.db = db
        self.vault = vault
        self.relayVault = relayVault
        self.trustStore = trustStore
        self.orchestrator = createSessionOrchestrator(callback: self)
        startNetworkPathMonitoring()
    }

    deinit {
        networkPathMonitor.cancel()
    }

    /// Phase 1C(#26): `NWPathMonitor`の生イベントをそのまま`orchestrator`へ転送する。
    private func startNetworkPathMonitoring() {
        networkPathMonitor.pathUpdateHandler = { [weak self] path in
            self?.orchestrator.notifyNetworkPathChanged(isSatisfied: path.status == .satisfied)
        }
        networkPathMonitor.start(queue: DispatchQueue(label: "tools.isekai.terminal.network-path-monitor"))
    }

    /// 接続を開始する。鍵認証の場合はCredentialVaultから秘密鍵を復号して使う。
    public func connect(cols: UInt32 = 80, rows: UInt32 = 24) {
        lastCols = cols
        lastRows = rows

        // Phase 1F-3(#50): Global default → Profile defaultの解決(Android版
        // `TerminalTabsViewModel.openTab`と同じ方針)。SGR解釈テーブルはRust側の
        // グローバル状態のため、接続開始前に適用しておけば以降の出力に反映される。
        resolveTheme().apply()

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

        switch profile.transportPreference {
        case .plainSsh:
            connectPlainSsh(auth: auth, jump: jump, cols: cols, rows: rows)
        case .isekaiHelperQuic:
            connectIsekaiPipeQuic(auth: auth, jump: jump, cols: cols, rows: rows, allowFallback: false)
        case .auto:
            connectIsekaiPipeQuic(auth: auth, jump: jump, cols: cols, rows: rows, allowFallback: true)
        case .isekaiStunP2pQuic:
            connectIsekaiStunP2p(auth: auth, jump: jump, cols: cols, rows: rows)
        case .isekaiLinkRelayQuic:
            connectIsekaiLinkRelay(auth: auth, jump: jump, cols: cols, rows: rows)
        case .isekaiHelperQuicMultipath:
            connectMultipathIsekaiPipeQuic(auth: auth, jump: jump, cols: cols, rows: rows)
        case .tsshdQuic:
            // Android版は対応済み(tsshdバイナリ経由の別実装、Phase 5B)だが、
            // iOS版のAndroid機能パリティ調査(#40〜#54)ではisekai-helper系を優先し
            // tsshd系は対象外にした(タスク未採番、優先度が低いため現時点では未実装)。
            fail(message: "この接続方式はiOS版ではまだ未対応です")
        }
    }

    // MARK: - Config構築(ネットワークに触れない純粋なマッピング)
    //
    // Android版`ConnectionProfile.toSshConfig`/`toIsekaiPipeQuicConfig`相当。実際の
    // `orchestrator.connect`呼び出し(Rust FFI越しのネットワーク処理)とは分離してあるため、
    // `internal`スコープのままテストから直接呼び出して(ネットワークに触れずに)検証できる。

    /// Android版`ConnectionProfile.toSshConfig`相当。
    func makeSshConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> SshConfig {
        // Android版と同じ方針: agent forwardingは公開鍵認証の場合のみ有効にする
        // (パスワード認証には転送すべき鍵材料が無いため)。
        let agentForward = profile.enableAgentForward && profile.keyEntryId != nil

        return SshConfig(
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
    }

    /// Phase 1A-9(#30): isekai-helper経由QUIC最小縦切り。Android版
    /// `ConnectionProfile.toIsekaiPipeQuicConfig`相当。ブートストラップ用の平文SSH接続
    /// (isekai-helperバイナリの配置)はRust側(`helper_bootstrap.rs`)が内部で行うため、
    /// Swift側は`SshConfig`と同様の接続情報(ポートフォワード/agent forward以外、
    /// `IsekaiPipeQuicConfig`にはまだ無い)を渡すだけでよい。
    func makeIsekaiPipeQuicConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> IsekaiPipeQuicConfig {
        IsekaiPipeQuicConfig(
            sshHost: profile.host,
            sshPort: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            jump: jump,
            bindPort: profile.helperBindPort.flatMap { UInt16(exactly: $0) }
        )
    }

    /// Phase 1E-5(#44): STUN+SSHランデブーP2P。Android版
    /// `ConnectionProfile.toIsekaiStunP2pConfig`相当。`profile.stunServers`
    /// (カンマ/空白区切りパース済み、空/未設定なら`defaultStunServer`1件、
    /// `ProfileDatabase.swift`参照)をそのまま渡す。
    func makeIsekaiStunP2pConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> IsekaiStunP2pConfig {
        IsekaiStunP2pConfig(
            sshHost: profile.host,
            sshPort: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            jump: jump,
            stunServers: profile.stunServers
        )
    }

    /// Phase 1E-6(#45): MASQUE relay P2P。Android版
    /// `ConnectionProfile.toIsekaiLinkRelayConfig`相当。`profile.relayJwt`は
    /// `relayVault`で暗号化して保存されているため、接続直前にここで復号する
    /// (Android版`TerminalTabsViewModel.connectTab`の`decryptRelayJwt`呼び出しと
    /// 同じタイミング)。復号失敗時(未設定・鍵ローテーション後等)はnilを返す。
    func makeIsekaiLinkRelayConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> IsekaiLinkRelayConfig? {
        guard let encryptedJwt = profile.relayJwt, let jwt = try? relayVault.decrypt(encryptedJwt) else {
            return nil
        }
        return IsekaiLinkRelayConfig(
            sshHost: profile.host,
            sshPort: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            jump: jump,
            relayAddr: profile.relayAddr ?? "",
            relaySni: profile.relaySni ?? "",
            relayJwt: jwt
        )
    }

    /// Phase 1E-7(#46): Tailscale⇔直接アドレスのマルチパス。Android版
    /// `ConnectionProfile.toMultipathIsekaiPipeQuicConfig`相当。`profile.directAddress`
    /// (path1、任意)が空/未設定ならmultipath化されずpath0のみで動く(通常のhelper QUICと
    /// 同等の耐性、Rust側のドキュメント参照)。物理Wi-Fi/セルラー無線への同時バインド
    /// (`wifiFd`/`cellularFd`等、#47の対象)は現時点では未実装のため常にnilを渡す
    /// (Android版もnoq側の既知バグ(issue #738)により現在は事実上no-opで、
    /// 効果があるのはpath0/path1のTailscale⇔直接アドレス切替のみ)。
    func makeMultipathIsekaiPipeQuicConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> MultipathIsekaiPipeQuicConfig {
        MultipathIsekaiPipeQuicConfig(
            sshHost: profile.host,
            sshPort: UInt16(profile.port),
            directHost: profile.directAddress?.trimmingCharacters(in: .whitespaces).isEmpty == false ? profile.directAddress : nil,
            cellularRemoteHost: profile.cellularRemoteAddress?.trimmingCharacters(in: .whitespaces).isEmpty == false ? profile.cellularRemoteAddress : nil,
            wifiFd: nil,
            wifiLocalIp: nil,
            cellularFd: nil,
            cellularLocalIp: nil,
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            jump: jump,
            bindPort: profile.helperBindPort.flatMap { UInt16(exactly: $0) }
        )
    }

    /// Android版`connect(tab, config)`相当。
    private func connectPlainSsh(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        let config = makeSshConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        do {
            try orchestrator.connect(config: config)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectIsekaiPipeQuic(tab, config)`/`connectIsekaiPipeQuicAuto(tab, config)`相当。
    private func connectIsekaiPipeQuic(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32, allowFallback: Bool) {
        let config = makeIsekaiPipeQuicConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        do {
            if allowFallback {
                try orchestrator.connectIsekaiPipeQuicAuto(config: config)
            } else {
                try orchestrator.connectIsekaiPipeQuic(config: config)
            }
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectIsekaiStunP2p(tab, config)`相当。
    private func connectIsekaiStunP2p(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        let config = makeIsekaiStunP2pConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        do {
            try orchestrator.connectIsekaiStunP2p(config: config)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectIsekaiLinkRelay(tab, config)`相当。
    private func connectIsekaiLinkRelay(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        guard let config = makeIsekaiLinkRelayConfig(auth: auth, jump: jump, cols: cols, rows: rows) else {
            fail(message: "relay JWTの復号に失敗しました")
            return
        }
        do {
            try orchestrator.connectIsekaiLinkRelay(config: config)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectMultipathIsekaiPipeQuic(tab, config)`相当。
    private func connectMultipathIsekaiPipeQuic(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        let config = makeMultipathIsekaiPipeQuicConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        do {
            try orchestrator.connectMultipathIsekaiPipeQuic(config: config)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Phase 1F-3(#50): プロファイル固有のテーマ指定があればそれを、無ければ
    /// アプリ全体の既定テーマ(`ProfileListView`の配色テーマ選択が書き込む
    /// `UserDefaults`)を使う(Android版`TerminalTabsViewModel.currentGlobalTheme`/
    /// `openTab`のGlobal default → Profile default解決と同じ方針)。
    func resolveTheme(defaults: UserDefaults = .standard) -> TerminalTheme {
        if let themeName = profile.themeName {
            return TerminalThemes.byName(themeName)
        }
        return TerminalThemes.byName(defaults.string(forKey: TerminalThemes.prefKey))
    }

    /// `keyEntryId`があればCredentialVaultから秘密鍵を復号して`.publicKey`認証を、
    /// 無ければ渡された`password`で`.password`認証を組み立てる。
    /// 失敗時は`fail(message:)`を呼びnilを返す。
    /// Android版`AuthValidator.validate`と同じ方針: パスワード認証で`password`が
    /// nil/空文字の場合はサーバーへ送らずここで拒否する(Codexアーキテクチャレビュー指摘:
    /// 旧実装は`password ?? ""`で空文字のまま認証を組み立てていた)。
    private func resolveAuth(keyEntryId: String?, password: String?, label: String) -> SshAuth? {
        guard let keyEntryId else {
            guard let password, !password.isEmpty else {
                fail(message: "\(label)のパスワードが必要です")
                return nil
            }
            return .password(password: password)
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
        orchestrator.send(data: data)
    }

    /// 打鍵列(KeySequence)を送信する。`applicationCursorMode`は新しいミラー状態を作らず、
    /// 既存のRust由来の状態(`uiState.latestScreenUpdate`、`TerminalView`が矢印キー描画等で
    /// 参照しているのと同じ値)をそのまま読む(Android版`TerminalTabsViewModel.sendKeySequenceToPane`
    /// と同じ方針)。`uiState`は`@MainActor`のため、このメソッド自身も`@MainActor`にする
    /// (UI操作からの呼び出しのみを想定し、`OrchestratorCallback`のRustスレッド発火メソッドとは
    /// 別に、このメソッドだけ`@MainActor`を明示できる)。
    @MainActor
    public func sendKeySequence(_ steps: [KeyStep]) {
        let applicationCursorMode = uiState.latestScreenUpdate?.applicationCursorMode ?? false
        send(KeySequenceCommands.toBytes(steps, applicationCursorMode: applicationCursorMode))
    }

    /// タスク#20: 動的resize(`TerminalScreenView`がview実サイズから算出したcols/rows)を
    /// Rust側へ転送する。`lastCols`/`lastRows`もここで更新しておくことで、以後
    /// `reconnect()`(手動再接続・バックグラウンド復帰)が接続直後の既定値(80x24)ではなく
    /// 直近の実サイズで再接続できる(codexレビュー指摘: 更新しないと再接続直後だけ
    /// 一瞬80x24に戻り、`resendSizeOnConnectionEstablished()`で補正されるまで
    /// 初期プロンプト等が誤った幅で折り返される)。
    public func resize(cols: UInt32, rows: UInt32) {
        lastCols = cols
        lastRows = rows
        orchestrator.resize(cols: cols, rows: rows)
    }

    public func disconnect() {
        orchestrator.disconnect()
    }

    /// `.reconnecting`中に「中止」操作から呼ぶ。Rust側の自動reconnectループを
    /// 停止する(Android版`actions.onCancelReconnect()`と同じ、`cancelReconnect()`
    /// 自体の要否判断はRust側が行う)。
    public func cancelReconnect() {
        orchestrator.cancelReconnect()
    }

    /// Phase 9-6(#16): 「今すぐWiFiに戻す」。マルチパス以外のセッションでは呼んでも
    /// Rust側で無視される(Android版`TerminalSession.forceReturnToWifi()`と同じ、
    /// 判断はRust側`RebindManager`に委ねる)。
    public func forceReturnToWifi() {
        orchestrator.forceReturnToWifi()
    }

    // MARK: - #20: バックグラウンド/フォアグラウンド遷移
    //
    // 生イベントをそのままRust側`SessionOrchestrator`へ転送するだけの薄いラッパー。
    // 「猶予内復帰か再接続が必要か」の判断はRust側が行う(`rust-ssot.md`) —
    // このクラス自身は分岐を持たない。呼び出し元は`TerminalTabsModel`。

    public func notifyDidEnterBackground(budgetMs: UInt32) {
        orchestrator.notifyDidEnterBackground(budgetMs: budgetMs)
    }

    public func notifyBackgroundBudgetExpired() {
        orchestrator.notifyBackgroundBudgetExpired()
    }

    public func notifyMemoryWarning() {
        orchestrator.notifyMemoryWarning()
    }

    public func notifyWillEnterForeground() {
        orchestrator.notifyWillEnterForeground()
    }

    // MARK: - #60: フォーカスレポーティング(`CSI ?1004`)
    //
    // OSのフォーカス変化(このタブがアクティブタブになった/でなくなった)をそのまま
    // Rust側`SessionOrchestrator`へ転送する薄いラッパー。フォーカスレポーティングが
    // 有効かどうか・実際に`CSI I`/`CSI O`を送るかどうかの判断はRust側(`Terminal`)が
    // 一元的に持つ(rust-ssot)。呼び出し元は`TerminalView`(`isActive`の変化)。

    public func notifyFocusChange(focused: Bool) {
        orchestrator.notifyFocusChange(focused: focused)
    }

    /// Phase 1C(#14): バックグラウンドからの復帰時や「再接続」ボタンから呼ぶ。
    /// 接続中/接続済みの間は二重接続を避けるため無視する(background/foreground
    /// 通知と手動ボタンの両方から呼ばれ得るため)。`connect()`と同じ
    /// cols/rows・認証情報でセッションを最初から作り直す(Rust側にresumeできる
    /// 論理セッションの概念はまだ無いため、既存セッションはただ破棄する)。
    /// Rust側`SessionOrchestrator::begin_connect`は`Connected`中の新規接続を
    /// (pending debounceのキャンセル+別セッションへの切り替えという内部経路のため)
    /// 意図的に許可しているが、ここでの`.connected`チェックはその判断を先取りしている
    /// のではなく、「バックグラウンド復帰通知と手動ボタンの両方から呼ばれ得るこの
    /// メソッド自身が誤って二重に走らないようにする」UI側の二重サブミット防止
    /// (Codexアーキテクチャレビューで指摘・確認済み、Android版`guardedConnect`と同種)。
    @MainActor
    public func reconnect() {
        switch uiState.state {
        case .connecting, .connected, .reconnecting:
            // .reconnecting中はRust側の自動reconnectループが既に動作中なので、
            // 手動での二重接続は行わない(中止したい場合はcancelReconnect()を使う)。
            return
        case .disconnected, .failed:
            uiState.state = .connecting
            uiState.latestScreenUpdate = nil
            connect(cols: lastCols, rows: lastRows)
        }
    }

    // MARK: - Host key

    /// 初回接続確認ダイアログで「信頼して接続」を選んだ時に呼ぶ。trust storeを更新するのみで、
    /// 接続自体は(Android版`TerminalSession.kt`の`trustNewHostKey()`と同様)ユーザーが手動で
    /// 再接続する想定(`.failed`状態で表示される`reconnectButton`、`reconnect()`参照)。
    @MainActor
    public func trustNewHostKey() {
        guard let prompt = uiState.newHostKeyPrompt else { return }
        uiState.newHostKeyPrompt = nil
        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: prompt.host, port: prompt.port)
        try? trustStore.trust(identifier: identifier, keyType: "ssh", fingerprint: prompt.fingerprint)
    }

    /// 初回接続確認ダイアログで「キャンセル」を選んだ時に呼ぶ。trust storeは更新せず切断する
    /// (Android版`TerminalSession.kt`の`dismissNewHostKeyPrompt()`と同じ方針)。
    @MainActor
    public func dismissNewHostKeyPrompt() {
        uiState.newHostKeyPrompt = nil
        disconnect()
    }

    /// Phase 1F-4(#51): スクロールバックのスワイプUI用。Android版
    /// `actions.onScrollbackCells`相当。セッション未確立時は空配列を返す。
    public func scrollbackCells(offset: UInt32, rows: UInt32) -> [CellData] {
        orchestrator.scrollbackCells(offset: offset, rows: rows)
    }

    /// Android版`uiState.scrollbackLen`相当。セッション未確立時は0を返す。
    public func scrollbackLen() -> UInt32 {
        orchestrator.scrollbackLen()
    }

    /// 保留中のagent署名要求に応答する(UI、MainActorから呼ぶ)。
    @MainActor
    public func respondToAgentSignRequest(approved: Bool) {
        uiState.pendingAgentSignRequest?.respond(approved)
        uiState.pendingAgentSignRequest = nil
    }

    // MARK: - trzsz(#25)

    /// アップロード開始。`url`はユーザーが`.fileImporter`で選択したファイル
    /// (security-scoped URL)。ファイルI/Oはメインスレッドをブロックしないよう
    /// バックグラウンドキューで行う。Android版`TerminalTabsViewModel.trzszStartUpload`
    /// と同じ「1チャンク先読みしてisLastを判定」方式(`Self.trzszSendChunked`)を使う。
    public func trzszStartUpload(url: URL) {
        guard let transferId = activeTrzszTransferId else { return }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            let didAccess = url.startAccessingSecurityScopedResource()
            defer { if didAccess { url.stopAccessingSecurityScopedResource() } }

            guard let fileHandle = try? FileHandle(forReadingFrom: url),
                  let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
                  let fileSize = (attrs[.size] as? NSNumber)?.uint64Value
            else {
                self.orchestrator.trzszCancel()
                return
            }
            defer { try? fileHandle.close() }

            self.activeTrzszFileName = url.lastPathComponent
            self.orchestrator.trzszAcceptUpload(fileName: url.lastPathComponent, fileSize: fileSize, mode: 0)
            Self.trzszSendChunked(
                readNext: { fileHandle.readData(ofLength: Self.trzszChunkSize) },
                send: { chunk, isLast in
                    self.orchestrator.trzszSendChunk(data: chunk, isLast: isLast)
                }
            )
        }
    }

    /// ダウンロード開始。書き込み先の一時ファイルのURLだけ確保して
    /// `trzszAcceptDownload`を呼ぶ(実際の書き込みは、Rust側が全量を貯めてから
    /// 一括で渡してくる`onDownloadComplete`で行う)。
    public func trzszStartDownload() {
        guard let transferId = activeTrzszTransferId else { return }
        // transferIdでnamespaceしたディレクトリに置く(同じ`suggestedName`の別転送/別タブが
        // 同じ一時パスへ書き込んで衝突するのを避けつつ、`.fileMover`に見せるファイル名は
        // 人間可読なままにする)。
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(
            "trzsz-\(transferId)", isDirectory: true
        )
        try? FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let tempURL = tempDir.appendingPathComponent(activeTrzszFileName ?? UUID().uuidString)
        // 空で作っておく: 0バイトの正常終了はRust側が`onDownloadComplete`自体を呼ばない
        // ため、これが無いと`completedDownloadURL`が存在しないファイルを指してしまう。
        // データが実際に届けば`onDownloadComplete`が上書きする。作成自体に失敗した場合
        // (ディレクトリ作成失敗を含む)は、成功扱いでも公開しないようフラグを立てる。
        let created = FileManager.default.createFile(atPath: tempURL.path, contents: nil)
        downloadTempURL = tempURL
        downloadWriteFailed = !created
        orchestrator.trzszAcceptDownload()
    }

    /// 進行中のtrzsz転送をキャンセルする。実際の`.done`遷移はRust側から
    /// `onTrzszStateChanged(.done(success: false, ...))`が来るのを待つ(Android版と
    /// 同じ、ここで即座にUI状態を書き換えない)。
    public func trzszCancel() {
        orchestrator.trzszCancel()
    }

    /// 転送完了シートを閉じる。Rust側の`current_transfer_id`等もクリアする
    /// (Android版`TerminalSession.kt`の`trzszDismiss()`と同じく`orchestrator.trzszDismiss()`
    /// を呼ぶ)。ダウンロード完了後の一時ファイルは、`.fileMover`で書き出し済みかどうかに
    /// 関わらずここで削除する(書き出し済みなら既に宛先へコピーされているため
    /// 一時ファイル自体はもう不要)。
    @MainActor
    public func trzszDismiss() {
        orchestrator.trzszDismiss()
        if let url = downloadTempURL {
            // 個々のファイルだけでなく、`trzszStartDownload()`が作った
            // transferId単位の一時ディレクトリごと削除する。
            try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
        }
        uiState.trzszState = nil
        uiState.completedDownloadURL = nil
        activeTrzszTransferId = nil
        activeTrzszMode = nil
        activeTrzszFileName = nil
        downloadTempURL = nil
        downloadWriteFailed = false
    }

    private func fail(message: String) {
        Task { @MainActor in self.uiState.state = .failed(message: message) }
    }

    // MARK: - OrchestratorCallback

    public func onConnectionStateChanged(state: ConnectionPublicState) {
        switch state {
        case .connecting:
            Task { @MainActor in self.uiState.state = .connecting }
        case .connected:
            Task { @MainActor in self.uiState.state = .connected }
        case .disconnected(let reason, let issueHint):
            Task { @MainActor in self.uiState.state = .disconnected(reason: reason, issueHint: issueHint) }
        case .error(let message):
            fail(message: message)
        case .reconnecting(let elapsedSecs, let timeoutSecs, let reason):
            Task { @MainActor in
                self.uiState.state = .reconnecting(elapsedSecs: elapsedSecs, timeoutSecs: timeoutSecs, reason: reason)
            }
        }
    }

    public func onScreenUpdate(update: ScreenUpdate) {
        Task { @MainActor in self.uiState.latestScreenUpdate = update }
    }

    /// ホスト鍵確認。Android版`TerminalSession.kt`の`onHostKey`(既定設定
    /// `autoTrustNewHostKeys=false`)と同じ方針: 初回接続(未知ホスト)は自動信頼せず、
    /// `uiState.newHostKeyPrompt`を立てて一旦接続を失敗させ、ユーザーが`trustNewHostKey()`
    /// で明示的に信頼した後の手動再接続に委ねる(未知ホストの自動trustは、悪性DNS/公衆Wi-Fi
    /// 経由のMITMで攻撃者鍵をそのまま初回登録してしまう実害のあるセキュリティギャップだった
    /// ——Codexアーキテクチャレビューで指摘、旧実装は自動trustしていた)。このcallbackは
    /// Rustスレッドから同期的にBoolを返す必要があるため、確認ダイアログの表示自体は
    /// `Task { @MainActor in }`経由でuiStateへ反映しつつ、戻り値はここで即座に`false`を返す。
    /// 渡された`host`/`port`をそのまま使う(`profile.host`ではなく)ことで、踏み台経由接続で
    /// ホップ先のホスト鍵が届いた場合にも正しいホストで検証できる(Android版`TerminalSession.kt`の
    /// `onHostKey(host, port, fingerprint)`と同じ方針)。
    public func onHostKey(host: String, port: UInt16, fingerprint: String) -> Bool {
        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: host, port: port)
        switch trustStore.verify(identifier: identifier, keyType: "ssh", fingerprint: fingerprint) {
        case .trustedMatch:
            return true
        case .unknownHost:
            Task { @MainActor in
                self.uiState.newHostKeyPrompt = NewHostKeyPrompt(host: host, port: port, fingerprint: fingerprint)
            }
            return false
        case .mismatch:
            fail(message: "ホスト鍵が変更されています(なりすましの可能性)。接続を中止しました。")
            return false
        }
    }

    public func onData(data: Data) {}

    public func onTrzszStateChanged(state: TrzszPublicState) {
        switch state {
        case .idle:
            Task { @MainActor in self.uiState.trzszState = nil }
        case .waitingUser(let transferId, let mode, let suggestedName, let expectedSize):
            activeTrzszTransferId = transferId
            activeTrzszMode = mode
            activeTrzszFileName = suggestedName
            Task { @MainActor in
                self.uiState.trzszState = .waitingUser(
                    transferId: transferId, mode: mode, suggestedName: suggestedName, expectedSize: expectedSize
                )
            }
        case .inProgress(let transferId, let mode, let fileName, let transferred, let total):
            Task { @MainActor in
                self.uiState.trzszState = .inProgress(
                    transferId: transferId, mode: mode, fileName: fileName, transferred: transferred, total: total
                )
            }
        case .done(let transferId, let success, let message):
            // ダウンロードが成功した場合、書き込み先のファイル自体は直前の
            // `onDownloadComplete`(Rust側が同じスレッドから同期的にこちらより先に
            // 呼ぶ、`orchestrator.rs::on_trzsz_finished`参照。ただし0バイトの場合は
            // 呼ばれない — `trzszStartDownload()`が空ファイルを事前に作っているため
            // それでも有効)で既に書き終わっている。実際の書き込みが失敗していた場合は
            // `downloadWriteFailed`により公開しない。
            let completedURL = (success && activeTrzszMode == "download" && !downloadWriteFailed) ? downloadTempURL : nil
            Task { @MainActor in
                self.uiState.trzszState = .done(transferId: transferId, success: success, message: message)
                self.uiState.completedDownloadURL = completedURL
            }
        }
    }

    /// ダウンロード完了。Rust側が全量を貯めてから一括で渡してくる(逐次チャンク書き込み
    /// ではない)。`trzszStartDownload()`が確保した`downloadTempURL`へ書き込む
    /// (`fileName`は常にnilで届くため使わない、`activeTrzszFileName`は既に
    /// `onTrzszStateChanged(.waitingUser)`で捕捉済み)。
    public func onDownloadComplete(fileName: String?, data: Data) {
        guard let url = downloadTempURL else { return }
        do {
            try data.write(to: url)
        } catch {
            downloadWriteFailed = true
        }
    }

    /// 物理multipathのfd取得自体がiOSでは未実装のため(タスク#12参照)、Android版
    /// `onNoViablePath`のようなupstream failover実装は対象外のまま(no-op)にしている。
    public func onNoViablePath() {}

    /// ポートフォワードの状態変化をログへ出力する(Android版`TerminalSession.kt`の
    /// `onForwardStateChanged`と同じくログのみ、UI状態には反映しない)。以前はno-opで
    /// Rustからの通知が失われていた(Codexアーキテクチャレビュー指摘)。
    public func onForwardStateChanged(id: String, state: ForwardState) {
        switch state {
        case .listening:
            Self.logger.info("port forward '\(id, privacy: .public)': listening")
        case .failed(let reason):
            Self.logger.warning("port forward '\(id, privacy: .public)': failed: \(reason, privacy: .public)")
        case .stopped:
            Self.logger.info("port forward '\(id, privacy: .public)': stopped")
        }
    }

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

    /// リモートがOSC 52またはtmux迂回チャンネル経由でクリップボードへの書き込みを要求した
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。opt-in設定のチェック・実際に`UIPasteboard`へ
    /// 書くかどうかの判断は`RemoteClipboardBridge`(UI設定であり`.claude/rules/rust-ssot.md`
    /// の対象外)に委譲する。Android版`TerminalSession.kt`の`onClipboardWrite`に相当。
    public func onClipboardWrite(payload: ClipboardPayload) {
        RemoteClipboardBridge.write(payload)
    }

    /// リモートがOSC 52 queryまたはtmux迂回チャンネルの`ClipboardPullRequest`で
    /// クリップボードの読み出しを要求した。Android版`TerminalSession.kt`の
    /// `onClipboardPullRequest`に相当。
    public func onClipboardPullRequest() -> ClipboardPayload? {
        RemoteClipboardBridge.pull()
    }

    // MARK: - RebindManager (PLAN.md Phase 9-6)
    //
    // iOS版のPhysicalPathProvider相当(IP_BOUND_IFベースのWiFi/セルラー個別バインド、#15)を
    // `physicalPathProvider`に実装済み。判断は一切せずfdを取得して返すだけという契約
    // (`rust-ssot.md`)で、取得できなければ`nil`を返す — RebindManager(Rust側)はfdが
    // 取れない場合を正常系として扱う設計になっており、日和見的にセルラーへの
    // フェイルオーバー/WiFiへの復帰が単に起きないだけで、既存のQUIC自身のローミング耐性
    // (`notifyNetworkPathChanged`)には影響しない。
    //
    // これらのcallbackはRustのspawn_blockingスレッドから同期的に呼ばれる
    // (`onHostKey`/`onAgentSignRequest`と同じ方式)。`physicalPathProvider`側も
    // `DispatchSemaphore`で同期的にブロックして待つ実装になっているため、
    // ここでは追加のスレッド橋渡しをせずそのまま返す。

    public func onRequestWifiFd() -> PlatformFd? {
        physicalPathProvider.acquireWifiFd().map { PlatformFd(fd: $0.fd, localIp: $0.localIp) }
    }

    public func onRequestCellularFd() -> PlatformFd? {
        physicalPathProvider.acquireCellularFd().map { PlatformFd(fd: $0.fd, localIp: $0.localIp) }
    }

    /// #19: `RebindManager`の状態が変化した。判定はこの値だけを見て行い(rust-ssot.md準拠)、
    /// UI側で独自のミラー状態は持たない(Android版`TerminalSession.kt`の
    /// `onRebindStateChanged`と同じ)。
    public func onRebindStateChanged(state: RebindPublicState) {
        Task { @MainActor in self.uiState.rebindState = state }
    }
}
