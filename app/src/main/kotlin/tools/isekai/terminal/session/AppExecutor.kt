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
}

data class UploadFile(val name: String, val size: Long, val stream: InputStream)
