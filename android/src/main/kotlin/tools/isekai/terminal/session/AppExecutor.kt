package tools.isekai.terminal.session

import android.net.Uri
import java.io.InputStream

/**
 * TerminalTabsViewModel が必要とする Android 側の副作用をすべて集約したインターフェース。
 * テストでは DumbAppExecutor に差し替えることで実機・Android フレームワーク不要になる。
 */
interface AppExecutor {
    /** バックグラウンドセッションサービスを起動・バインドする。 */
    fun ensureServiceRunning()
    /** SSH 接続済みをシステム通知へ伝える。 */
    fun notifyConnected(host: String)
    /** SSH 切断をシステム通知へ伝える。 */
    fun notifyDisconnected()
    /**
     * 複数タブ共有時の集約通知を更新する。[totalCount] が 0 の場合は FGS を停止してよい。
     * 単一セッションの [notifyConnected]/[notifyDisconnected] とは独立した経路。
     */
    fun updateSessionsSummary(connectedCount: Int, totalCount: Int)
    /** ネットワーク変化のコールバックを登録する。 */
    fun registerNetworkCallbacks(onAvailable: () -> Unit, onLost: () -> Unit)
    /** ネットワーク変化のコールバックを解除する。 */
    fun unregisterNetworkCallbacks()
    /** 指定 keyId の秘密鍵を復号して PEM バイト列で返す。 */
    suspend fun loadKeyPem(keyId: Long): ByteArray
    /**
     * [ConnectionProfile.relayJwt][tools.isekai.terminal.data.ConnectionProfile.relayJwt]
     * (Roomには`RelayCredentialVault`で暗号化して保存済み)を復号して平文JWTを返す。
     * relay接続(`toIsekaiLinkRelayConfig`)の直前にのみ呼ぶこと。
     */
    fun decryptRelayJwt(ciphertext: String): String
    /** アップロード対象 URI を開いてメタデータ＋InputStream を返す。null なら開けなかった。 */
    suspend fun openUploadFile(uri: Uri): UploadFile?
    /** サービスバインドを解除する (onCleared から呼ぶ)。 */
    fun release()
    /** ダウンロードファイルを端末のDownloadsフォルダに保存する。 */
    suspend fun saveDownloadFile(fileName: String, data: ByteArray)
    /**
     * Phase 9-4（実験的機能）: Wi-Fi/セルラー物理無線にそれぞれ明示的にバインドした
     * ソケットの fd を取得する。両方/片方が取得できないことは正常系（Tailscale稼働中
     * 等）なので、呼び出し側は結果の null を許容すること。返り値の[PhysicalMultipathAcquisition.handle]
     * は呼び出し側(pane)が所有し、そのpaneの接続試行が終わったら必ず`close()`すること
     * (Task #10: プロセス単位グローバル状態を廃し、per-pane handleに所有権を渡す設計)。
     */
    suspend fun acquirePhysicalMultipathFds(): PhysicalMultipathAcquisition
    /**
     * 「WiFiは繋がっているがupstreamが死んでいる」検知の監視を開始する。
     * [onWifiUpstreamBroken] は検証失敗を検知した瞬間に呼ばれる（edge-triggered）。
     * 返り値の[AutoCloseable]は呼び出し側(pane)が所有し、監視を止めたくなったら`close()`すること。
     */
    fun registerUpstreamFailoverMonitor(onWifiUpstreamBroken: () -> Unit): AutoCloseable
    /**
     * WiFi/セルラーに明示的にバインドしたソケットの生fd取得を、1つのpane/session分だけ
     * まとめて行うためのファクトリ。RebindManager(Rust側)がセッション中に何度も
     * wifi/cellular fdを要求してくる(疎通確認用・本番rebind用で毎回別々に取得する、fd所有権
     * ポリシー)ため、[RebindFdSource]はpaneのセッションと同じ寿命を持つ1インスタンスとして
     * 呼び出し側が保持し、セッション終了時に`close()`すること。
     */
    fun createRebindFdSource(): RebindFdSource
}

data class UploadFile(val name: String, val size: Long, val stream: InputStream)

/** [AppExecutor.acquirePhysicalMultipathFds] の返り値。[handle]は呼び出し側が所有・解放する。 */
data class PhysicalMultipathAcquisition(val fds: PhysicalMultipathFds, val handle: AutoCloseable)

/**
 * 1つのpane/sessionに紐づくWiFi/セルラーfdの取得元。[close]を呼ぶまで何度でも
 * [acquireWifiFd]/[acquireCellularFd]を呼んでよい(呼ぶたびに新規fdを取得する、fd所有権
 * ポリシー)。[close]後の呼び出しはnullを返し、新規リソースを確保しない。
 */
interface RebindFdSource : AutoCloseable {
    /** #10/#20: WiFiに明示的にバインドしたソケットの生fdとローカルIPを取得する。取得できなければnull。 */
    suspend fun acquireWifiFd(): Pair<Int, String>?
    /** セルラーに明示的にバインドしたソケットの生fdとローカルIPを取得する。取得できなければnull。 */
    suspend fun acquireCellularFd(): Pair<Int, String>?
}
