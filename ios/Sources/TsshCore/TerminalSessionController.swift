import Foundation
import TsshCoreLogic

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
    /// Phase 1C(#25): trzszファイル転送の状態。非nilの間、転送シートを表示する。
    @Published public internal(set) var trzszState: TrzszUiState?
    /// Phase 1C(#25): ダウンロード完了後、ユーザーがFilesアプリ等へ保存できる
    /// 一時ファイルのURL。`trzszState`が`.done(success: true, ...)`かつダウンロード
    /// だった場合のみ設定される。
    @Published public internal(set) var completedDownloadURL: URL?

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

/// Phase 1A-9(#30): `SshSession`/`HelperQuicSession`など、生成される各セッション型は
/// 個別の`XxxSessionProtocol`にしか準拠していない(共通の親プロトコルが無い)ため、
/// `TerminalSessionController`が接続方式を問わず同じ`send`/`resize`/`disconnect`
/// 呼び出しで扱えるよう、この最小限のプロトコルへ同一モジュール内で事後適合させる。
private protocol ActiveTerminalSession: AnyObject {
    func send(data: Data)
    func resize(cols: UInt32, rows: UInt32)
    func disconnect()
    func scrollbackCells(offset: UInt32, rows: UInt32) -> [CellData]
    func scrollbackLen() -> UInt32
    func trzszAcceptUpload(transferId: String, fileName: String, fileSize: UInt64, mode: UInt32)
    func trzszSendChunk(transferId: String, data: Data, isLast: Bool)
    func trzszAcceptDownload(transferId: String)
    func trzszCancel(transferId: String)
}
extension SshSession: ActiveTerminalSession {}
extension HelperQuicSession: ActiveTerminalSession {}
extension IsekaiStunP2pSession: ActiveTerminalSession {}
extension IsekaiLinkRelaySession: ActiveTerminalSession {}
extension MultipathHelperQuicSession: ActiveTerminalSession {}

/// Android版`ConnectionProfile.DEFAULT_STUN_SERVER`と同じ既定STUNサーバー
/// (双方が同じSTUNサーバーを使う必要は無いため、単なるデフォルト値)。
let defaultStunServer = "stun.l.google.com:19302"

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
    private let relayVault: RelayCredentialVault
    private let trustStore: SshHostTrustStore
    private var session: ActiveTerminalSession?
    /// Phase 1C(#14): `reconnect()`が最後に使ったcols/rowsで再接続できるように保持する。
    private var lastCols: UInt32 = 80
    private var lastRows: UInt32 = 24
    /// Phase 1C(#25): 進行中のtrzsz転送のID/mode/表示名。`onTrzszRequest`で設定し、
    /// `trzszDismiss()`でクリアする。Rustスレッド(callback)とUI操作スレッドの両方から
    /// 触るため、単純な代入のみで完結する範囲でしか使わない(複雑な排他制御はしない)。
    private var activeTrzszTransferId: String?
    private var activeTrzszMode: String?
    private var activeTrzszFileName: String?
    /// Phase 1C(#25): ダウンロード中に書き込む一時ファイル。`trzszStartDownload()`で
    /// 開き、`onTrzszDownloadChunk`(isLast)/`onTrzszFinished`のどちらか先に来た方で
    /// 閉じる(両方から呼ばれても2回目はno-op、`FileHandle.closeFile()`は複数回呼んでも
    /// 安全)。
    private var downloadFileHandle: FileHandle?
    private var downloadTempURL: URL?

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
            connectHelperQuic(auth: auth, jump: jump, cols: cols, rows: rows, allowFallback: false)
        case .auto:
            connectHelperQuic(auth: auth, jump: jump, cols: cols, rows: rows, allowFallback: true)
        case .isekaiStunP2pQuic:
            connectIsekaiStunP2p(auth: auth, jump: jump, cols: cols, rows: rows)
        case .isekaiLinkRelayQuic:
            connectIsekaiLinkRelay(auth: auth, jump: jump, cols: cols, rows: rows)
        case .isekaiHelperQuicMultipath:
            connectMultipathHelperQuic(auth: auth, jump: jump, cols: cols, rows: rows)
        case .tsshdQuic:
            // Android版は対応済み(tsshdバイナリ経由の別実装、Phase 5B)だが、
            // iOS版のAndroid機能パリティ調査(#40〜#54)ではisekai-helper系を優先し
            // tsshd系は対象外にした(タスク未採番、優先度が低いため現時点では未実装)。
            fail(message: "この接続方式はiOS版ではまだ未対応です")
        }
    }

    // MARK: - Config構築(ネットワークに触れない純粋なマッピング)
    //
    // Android版`ConnectionProfile.toSshConfig`/`toHelperQuicConfig`相当。実際の
    // セッション生成(`createSshSession`/`createHelperQuicSession`、Rust FFI越しの
    // ネットワーク処理)とは分離してあるため、`internal`スコープのままテストから
    // 直接呼び出して(ネットワークに触れずに)検証できる。

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
    /// `ConnectionProfile.toHelperQuicConfig`相当。ブートストラップ用の平文SSH接続
    /// (isekai-helperバイナリの配置)はRust側(`helper_bootstrap.rs`)が内部で行うため、
    /// Swift側は`SshConfig`と同様の接続情報(ポートフォワード/agent forward以外、
    /// `HelperQuicConfig`にはまだ無い)を渡すだけでよい。
    func makeHelperQuicConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> HelperQuicConfig {
        HelperQuicConfig(
            sshHost: profile.host,
            sshPort: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            jump: jump
        )
    }

    /// Phase 1E-5(#44): STUN+SSHランデブーP2P。Android版
    /// `ConnectionProfile.toIsekaiStunP2pConfig`相当。`profile.stunServer`が
    /// 未設定/空文字なら`defaultStunServer`を使う(Android版`DEFAULT_STUN_SERVER`と同じ方針、
    /// 双方が同じSTUNサーバーを使う必要は無いため単なるデフォルト値)。
    func makeIsekaiStunP2pConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> IsekaiStunP2pConfig {
        let stunServer = profile.stunServer?.trimmingCharacters(in: .whitespaces)
        return IsekaiStunP2pConfig(
            sshHost: profile.host,
            sshPort: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            jump: jump,
            stunServer: (stunServer?.isEmpty ?? true) ? defaultStunServer : stunServer!
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
    /// `ConnectionProfile.toMultipathHelperQuicConfig`相当。`profile.directAddress`
    /// (path1、任意)が空/未設定ならmultipath化されずpath0のみで動く(通常のhelper QUICと
    /// 同等の耐性、Rust側のドキュメント参照)。物理Wi-Fi/セルラー無線への同時バインド
    /// (`wifiFd`/`cellularFd`等、#47の対象)は現時点では未実装のため常にnilを渡す
    /// (Android版もnoq側の既知バグ(issue #738)により現在は事実上no-opで、
    /// 効果があるのはpath0/path1のTailscale⇔直接アドレス切替のみ)。
    func makeMultipathHelperQuicConfig(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) -> MultipathHelperQuicConfig {
        MultipathHelperQuicConfig(
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
            jump: jump
        )
    }

    /// Android版`connect(tab, config)`相当。
    private func connectPlainSsh(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        let config = makeSshConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        let newSession = createSshSession(config: config)
        session = newSession
        do {
            try newSession.connect(callback: self)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectHelperQuic(tab, config)`/`connectHelperQuicAuto(tab, config)`相当。
    private func connectHelperQuic(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32, allowFallback: Bool) {
        let config = makeHelperQuicConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        let newSession = createHelperQuicSession(config: config)
        session = newSession
        do {
            if allowFallback {
                try newSession.connectAuto(callback: self)
            } else {
                try newSession.connect(callback: self)
            }
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectIsekaiStunP2p(tab, config)`相当。
    private func connectIsekaiStunP2p(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        let config = makeIsekaiStunP2pConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        let newSession = createIsekaiStunP2pSession(config: config)
        session = newSession
        do {
            try newSession.connect(callback: self)
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
        let newSession = createIsekaiLinkRelaySession(config: config)
        session = newSession
        do {
            try newSession.connect(callback: self)
        } catch {
            fail(message: "\(error)")
        }
    }

    /// Android版`connectMultipathHelperQuic(tab, config)`相当。
    private func connectMultipathHelperQuic(auth: SshAuth, jump: JumpConfig?, cols: UInt32, rows: UInt32) {
        let config = makeMultipathHelperQuicConfig(auth: auth, jump: jump, cols: cols, rows: rows)
        let newSession = createMultipathHelperQuicSession(config: config)
        session = newSession
        do {
            try newSession.connect(callback: self)
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

    /// Phase 1C(#14): バックグラウンドからの復帰時や「再接続」ボタンから呼ぶ。
    /// 接続中/接続済みの間は二重接続を避けるため無視する(background/foreground
    /// 通知と手動ボタンの両方から呼ばれ得るため)。`connect()`と同じ
    /// cols/rows・認証情報でセッションを最初から作り直す(Rust側にresumeできる
    /// 論理セッションの概念はまだ無いため、既存セッションはただ破棄する)。
    @MainActor
    public func reconnect() {
        switch uiState.state {
        case .connecting, .connected:
            return
        case .disconnected, .failed:
            uiState.state = .connecting
            uiState.latestScreenUpdate = nil
            connect(cols: lastCols, rows: lastRows)
        }
    }

    /// Phase 1F-4(#51): スクロールバックのスワイプUI用。Android版
    /// `actions.onScrollbackCells`相当。セッション未確立時は空配列を返す。
    public func scrollbackCells(offset: UInt32, rows: UInt32) -> [CellData] {
        session?.scrollbackCells(offset: offset, rows: rows) ?? []
    }

    /// Android版`uiState.scrollbackLen`相当。セッション未確立時は0を返す。
    public func scrollbackLen() -> UInt32 {
        session?.scrollbackLen() ?? 0
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
                self.session?.trzszCancel(transferId: transferId)
                return
            }
            defer { try? fileHandle.close() }

            self.activeTrzszFileName = url.lastPathComponent
            self.session?.trzszAcceptUpload(
                transferId: transferId, fileName: url.lastPathComponent, fileSize: fileSize, mode: 0
            )
            Self.trzszSendChunked(
                readNext: { fileHandle.readData(ofLength: Self.trzszChunkSize) },
                send: { chunk, isLast in
                    self.session?.trzszSendChunk(transferId: transferId, data: chunk, isLast: isLast)
                }
            )
        }
    }

    /// ダウンロード開始。受信データを書き込む一時ファイルを開いてから
    /// `trzszAcceptDownload`を呼ぶ(受信チャンクは`onTrzszDownloadChunk`で届く)。
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
        FileManager.default.createFile(atPath: tempURL.path, contents: nil)
        guard let handle = try? FileHandle(forWritingTo: tempURL) else {
            session?.trzszCancel(transferId: transferId)
            return
        }
        downloadFileHandle = handle
        downloadTempURL = tempURL
        session?.trzszAcceptDownload(transferId: transferId)
    }

    /// 進行中のtrzsz転送をキャンセルする。実際の`.done`遷移はRust側から
    /// `onTrzszFinished(success: false, ...)`が来るのを待つ(Android版と同じ、
    /// ここで即座にUI状態を書き換えない)。
    public func trzszCancel() {
        guard let transferId = activeTrzszTransferId else { return }
        session?.trzszCancel(transferId: transferId)
    }

    /// 転送完了シートを閉じる(クライアント側のみの状態リセット、Rust APIコールなし)。
    /// ダウンロード完了後の一時ファイルは、`.fileMover`で書き出し済みかどうかに
    /// 関わらずここで削除する(書き出し済みなら既に宛先へコピーされているため
    /// 一時ファイル自体はもう不要)。
    @MainActor
    public func trzszDismiss() {
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
        downloadFileHandle = nil
        downloadTempURL = nil
    }

    private func closeDownloadHandleIfNeeded() {
        try? downloadFileHandle?.close()
        downloadFileHandle = nil
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

    /// リモートからtrzsz転送要求が届いた。Android版`TerminalSession.kt`の
    /// `onTrzszRequest`→`TrzszUiState.WaitingUser`と同じ、まずはユーザー確認待ちにする。
    public func onTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: UInt64?) {
        activeTrzszTransferId = transferId
        activeTrzszMode = mode
        activeTrzszFileName = suggestedName
        Task { @MainActor in
            self.uiState.trzszState = .waitingUser(
                transferId: transferId, mode: mode, suggestedName: suggestedName, expectedSize: expectedSize
            )
        }
    }

    /// ダウンロード中のデータチャンク。`trzszStartDownload()`が開いた一時ファイルへ
    /// 逐次書き込む(Rustスレッドから直接呼ばれるため、MainActorへはホップしない)。
    public func onTrzszDownloadChunk(transferId: String, data: Data, isLast: Bool) {
        downloadFileHandle?.write(data)
        if isLast {
            closeDownloadHandleIfNeeded()
        }
    }

    public func onTrzszProgress(transferId: String, transferred: UInt64, total: UInt64?) {
        let mode = activeTrzszMode ?? ""
        let fileName = activeTrzszFileName
        Task { @MainActor in
            self.uiState.trzszState = .inProgress(
                transferId: transferId, mode: mode, fileName: fileName, transferred: transferred, total: total
            )
        }
    }

    /// 転送完了。ダウンロードが成功した場合のみ、一時ファイルのURLをUIへ渡して
    /// `.fileMover`での保存を可能にする(Android版がアプリのDL完了通知経由でSAF保存を
    /// 促すのと同じ役割)。
    public func onTrzszFinished(transferId: String, success: Bool, message: String?) {
        closeDownloadHandleIfNeeded()
        let completedURL = (success && activeTrzszMode == "download") ? downloadTempURL : nil
        Task { @MainActor in
            self.uiState.trzszState = .done(transferId: transferId, success: success, message: message)
            self.uiState.completedDownloadURL = completedURL
        }
    }

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
