package tools.isekai.terminal.session

import android.net.Uri
import java.io.InputStream

/**
 * TerminalViewModel が必要とする Android 側の副作用をすべて集約したインターフェース。
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
    /** アップロード対象 URI を開いてメタデータ＋InputStream を返す。null なら開けなかった。 */
    suspend fun openUploadFile(uri: Uri): UploadFile?
    /** サービスバインドを解除する (onCleared から呼ぶ)。 */
    fun release()
    /** ダウンロードファイルを端末のDownloadsフォルダに保存する。 */
    suspend fun saveDownloadFile(fileName: String, data: ByteArray)
    /**
     * Phase 9-4（実験的機能）: Wi-Fi/セルラー物理無線にそれぞれ明示的にバインドした
     * ソケットの fd を取得する。両方/片方が取得できないことは正常系（Tailscale稼働中
     * 等）なので、呼び出し側は結果の null を許容すること。
     */
    suspend fun acquirePhysicalMultipathFds(): PhysicalMultipathFds
    /** [acquirePhysicalMultipathFds] で保持したネットワークリクエストを解除する。 */
    fun releasePhysicalMultipathFds()
    /**
     * 「WiFiは繋がっているがupstreamが死んでいる」検知の監視を開始する。
     * [onWifiUpstreamBroken] は検証失敗を検知した瞬間に呼ばれる（edge-triggered）。
     */
    fun registerUpstreamFailoverMonitor(onWifiUpstreamBroken: () -> Unit)
    /** [registerUpstreamFailoverMonitor] の監視を解除する。 */
    fun unregisterUpstreamFailoverMonitor()
    /**
     * セルラーに明示的にバインドしたソケットの生fdとローカルIPを取得する
     * （[acquirePhysicalMultipathFds] のセルラー単体版）。取得できなければnull
     * （Tailscale稼働中でbindSocketが失敗する等、正常系として許容する）。
     */
    suspend fun acquireCellularFd(): Pair<Int, String>?
}

data class UploadFile(val name: String, val size: Long, val stream: InputStream)
