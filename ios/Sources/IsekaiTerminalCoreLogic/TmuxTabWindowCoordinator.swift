import Foundation

/// タスク#3: Android版タスク#60(`TerminalTabsViewModel.maybeEnsureTmuxTabWindow` +
/// `util/ClientIdentity.kt` + `data/TmuxTabLocator.kt`)のiOS移植。
///
/// tmux session group(`rust-core/src/tmux_session.rs`)は「同じデバイスからの再接続は
/// 常に同じグループメンバー(セッション名)に戻り、別デバイスは別のグループメンバーに
/// なる」という前提で設計されている。どのtmuxウィンドウを選ぶか・新規作成するか等の
/// 判断は一切ここに置かず、`SessionOrchestrator.ensureTmuxTabWindow`(Rust側)に委譲する
/// (`.claude/rules/rust-ssot.md`)。このファイルは
///
/// 1. アプリインストール固有の`client_id`を(無ければ)生成してRustへ渡す
/// 2. 永続化済みの`existing_tag`があれば読んで渡す
/// 3. Rustが返した`tag`を永続化ストアへ書き戻す
/// 4. UI表示用の最小限のラベル("tmux:2"等)を組み立てる
///
/// という薄い配線だけを行う。GRDB/UserDefaults等のApple専用永続化には依存せず、
/// 全てプロトコル越しに注入するため、`IsekaiTerminalCoreLogicTests`でLinux上でも
/// フェイク実装を使ってテストできる(`SshHostTrustStore`とは違いGRDBが必要な
/// テーブルはApple専用の`IsekaiTerminalCore`側に置くしかないが、決定ロジック自体は
/// ここに閉じ込める、という考え方は同じ)。

/// このアプリインストール固有の永続的なクライアント識別トークンの読み書き。
/// 実体(iOSでは`UserDefaults`、Android版`SharedPreferences("isekai_terminal_ui")`と
/// 対称)は`IsekaiTerminalCore`側の実装に委ねる。
public protocol ClientIdentityStore {
    func readClientId() -> String?
    func writeClientId(_ value: String)
}

/// Android版`ClientIdentity.getOrCreate`相当。端末固有の識別子(ANDROID_ID相当)では
/// なくアプリ生成のランダムUUIDを使う: アプリをアンインストール/再インストールすれば
/// 新しい値になる(=前回とは別グループメンバー扱いになる)方が、プライバシー上の
/// 懸念に左右されない安定した実装になるため(Android版と同じ判断)。
public enum ClientIdentity {
    public static func getOrCreate(
        store: ClientIdentityStore,
        makeId: () -> String = { UUID().uuidString }
    ) -> String {
        if let existing = store.readClientId() {
            return existing
        }
        let fresh = makeId()
        store.writeClientId(fresh)
        return fresh
    }
}

/// プロファイル(の主接続)が使っているtmux session groupのウィンドウを長期的に指す
/// タグの永続化。Android版`TmuxTabLocator.kt`/`TmuxTabLocatorDao`と対称、キーは
/// タブID(プロセス内限定・アプリ再起動を跨いで復元されない)ではなく、安定した
/// `profileId`にする(Android版と同じ判断 — このプロジェクトにはタブ一覧の永続化/
/// 復元機能が無いため、永続化キーには代わりにプロファイル単位の粒度を使う)。
///
/// `tag`だけを永続化し、ウィンドウインデックス等の揮発性の値は保持しない
/// (`TmuxCoordinates`/Android版`TmuxTabLocator`と同じ方針)。
public protocol TmuxTabLocatorStore {
    func findTag(forProfileId profileId: Int64) throws -> String?
    func saveTag(_ tag: String, forProfileId profileId: Int64) throws
}

/// `SessionOrchestrator.ensureTmuxTabWindow`(UniFFI生成)を最小限抽象化した
/// プロトコル。生成された`SessionOrchestratorProtocol`はこれ以外にも多数の
/// メソッドを持つため、テスト用フェイクがこの1メソッドだけを実装すればよいように
/// 独立させている(`extension SessionOrchestrator: TmuxTabWindowResolving {}`は
/// 実装をそのまま満たすため、本体側で追加のコードは不要)。
public protocol TmuxTabWindowResolving {
    func ensureTmuxTabWindow(
        profileIdentity: String,
        clientId: String,
        existingTag: String?
    ) async throws -> TmuxTabWindowInfo
}

extension SessionOrchestrator: TmuxTabWindowResolving {}

/// `TmuxTabWindowCoordinator.ensureWindow`の結果。`label`はAndroid版
/// `TerminalHostScreen.kt`の`" · tmux:$windowIndex"`サフィックスと対称の、
/// UIへそのまま出してよい表示用文字列("tmux:2"のように、先頭の"tmux:"は
/// 呼び出し側が付け足す必要が無いようここで組み立て済み)。
public struct TmuxTabWindowResult: Equatable, Sendable {
    public let label: String
    public let info: TmuxTabWindowInfo

    public init(label: String, info: TmuxTabWindowInfo) {
        self.label = label
        self.info = info
    }
}

/// タスク#3(#60のiOS移植)本体。primary paneの接続が確立した際に呼ぶ想定
/// (split paneは対象外 — Android版と同じMVPスコープ判断、`rust-core/src/tmux_session.rs`
/// のモジュールdoc参照。iOS版は現状split pane自体が無く、1タブ=1セッションのため
/// このスコープ制限は呼び出し元では特に意識する必要が無い)。
public enum TmuxTabWindowCoordinator {
    /// - Parameters:
    ///   - profileId: 安定なプロファイル識別子(iOS版`ConnectionProfile.id`、GRDBの
    ///     自動採番主キー)。ここから`"profile:\(profileId)"`という`profileIdentity`
    ///     文字列を組み立ててRustへ渡す(Android版`"profile:${profile.id}"`と同じ形式)。
    ///     永続化ストア自体は`profileId`をそのままキーに使う(Android版Roomの
    ///     `profile_id`列と同じ)。
    ///   - resolver: 通常は`SessionOrchestrator`(実際は`orchestrator`プロパティ)。
    ///   - clientIdentityStore/locatorStore: Apple専用実装(`UserDefaults`/GRDB)を
    ///     呼び出し側が注入する。
    ///   - makeId: テストからUUID生成を差し替えられるようにするためのフック。
    public static func ensureWindow(
        profileId: Int64,
        resolver: TmuxTabWindowResolving,
        clientIdentityStore: ClientIdentityStore,
        locatorStore: TmuxTabLocatorStore,
        makeId: () -> String = { UUID().uuidString }
    ) async throws -> TmuxTabWindowResult {
        let profileIdentity = "profile:\(profileId)"
        let clientId = ClientIdentity.getOrCreate(store: clientIdentityStore, makeId: makeId)
        let existingTag = try locatorStore.findTag(forProfileId: profileId)
        let info = try await resolver.ensureTmuxTabWindow(
            profileIdentity: profileIdentity,
            clientId: clientId,
            existingTag: existingTag
        )
        try locatorStore.saveTag(info.tag, forProfileId: profileId)
        return TmuxTabWindowResult(label: "tmux:\(info.windowIndex)", info: info)
    }
}
