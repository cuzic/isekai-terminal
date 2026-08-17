package tools.isekai.terminal

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Binder
import android.os.IBinder

/**
 * ターミナルセッションを保持する Foreground Service。
 *
 * - Activity が破棄（画面回転・バックグラウンド移行）されてもセッションを継続する
 * - Android 14 以降では foregroundServiceType の宣言が必須
 */
class TerminalSessionService : Service() {

    inner class SessionBinder : Binder() {
        fun getService(): TerminalSessionService = this@TerminalSessionService
    }

    private val binder = SessionBinder()
    private var sessionLabel: String = "接続なし"

    fun notifyConnected(host: String) {
        updateNotification("接続中: $host")
    }
    fun notifyDisconnected() {
        updateNotification("切断済み")
    }

    /**
     * 複数タブ共有時の集約通知。
     *
     * [totalCount] が 0 になった（＝最後のタブが閉じられた）場合のみ自身を停止する。
     * それ以外は「Nセッション接続中」という1枚の通知に集約する。
     */
    fun updateSessionsSummary(connectedCount: Int, totalCount: Int) {
        if (totalCount <= 0) {
            stopSelf()
            return
        }
        val label = if (connectedCount > 0) "${connectedCount}セッション接続中" else "${totalCount}タブ（切断済み）"
        updateNotification(label)
    }

    override fun onBind(intent: Intent): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent == null) {
            // プロセスがkillされた後にSTART_STICKYの仕様でOSが自動再起動した呼び出し
            // (再起動時は元のIntentが再送されずnullになる)。プロセス全体が死んだ時点で
            // Rust側のセッション状態(SessionOrchestrator等)もJVMごと消えており、この
            // サービス単体を空のまま前面化しても復元できるセッションは無い——実際の
            // 「黙示的セッション再アタッチ」(タスク#14)はTerminalTabsViewModelが
            // コールドスタート時(アプリを開き直した時)に行う。
            //
            // ここで無条件にstartForeground()していた旧実装は、Android 15以降
            // dataSync/mediaProcessing等のFGSがバックグラウンドから起動されると
            // システムに短時間で強制終了され(ForegroundServiceDidNotStopInTimeException、
            // 当時の型はdataSyncだった。現在はspecialUseへ変更済みだが、
            // このnullチェック自体は型に関係なく必要な修正)、
            // それによる2回目の自動再起動がmAllowStartForeground=falseで即
            // ForegroundServiceStartNotAllowedExceptionとなり、"アプリが繰り返し
            // 停止しています"の無限クラッシュループに陥ることを実機で確認した
            // (2026-07-27)。中身の無い再起動はそもそも試みず即座に自分を停止する。
            stopSelf()
            return START_NOT_STICKY
        }
        val label = intent.getStringExtra(EXTRA_SESSION_LABEL) ?: "SSH セッション"
        startForegroundWithNotification(label)
        return START_STICKY
    }

    fun updateNotification(label: String) {
        sessionLabel = label
        val manager = getSystemService(NotificationManager::class.java)
        manager.notify(NOTIFICATION_ID, buildNotification(label))
    }

    override fun onDestroy() {
        // 項目2: サービスが正規のライフサイクル経由で終了することを示す「正常終了
        // マーカー」を書く。OEMのバックグラウンドキラー等がプロセスを直接killする
        // 場合はonDestroyが呼ばれずこのマーカーが書かれないため、次回起動時に
        // 「新鮮なreattachレコードあり && マーカー無し」から予期しないkillを検出できる
        // (`background_reliability_policy.rs`のモジュールdoc、`TerminalTabsViewModel`
        // 側の突き合わせロジック参照)。
        markCleanShutdown(this)
        super.onDestroy()
    }

    // ── 通知 ──────────────────────────────────────────────

    private fun startForegroundWithNotification(label: String) {
        val notification = buildNotification(label)
        // Android 14+: foregroundServiceType は Manifest で宣言（specialUse）
        startForeground(NOTIFICATION_ID, notification)
    }

    private fun buildNotification(label: String): Notification {
        val tapIntent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val tapPending = PendingIntent.getActivity(
            this, 0, tapIntent, PendingIntent.FLAG_IMMUTABLE
        )

        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(label)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentIntent(tapPending)
            .setOngoing(true)
            .build()
    }

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "SSH セッション",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "SSH / Mosh セッションのバックグラウンド接続"
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    companion object {
        const val EXTRA_SESSION_LABEL = "session_label"
        private const val CHANNEL_ID = "isekai_terminal_session_main"
        private const val NOTIFICATION_ID = 1002

        // ── 項目2: 正常終了マーカー ──────────────────────────────
        // OEMバッテリー最適化への案内UI(`background_reliability_policy.rs`参照)の
        // ための「予期しないkill」検出に使う。`onDestroy`到達時にのみ書き込み、
        // `TerminalTabsViewModel`が起動時に消費する。`onTaskRemoved`では書かない
        // (タスクがrecentsからスワイプ削除されてもFGSは止めない設計であり、その
        // 時点ではまだ「正常終了」ではない——直後にOEMのバックグラウンドキラーに
        // プロセスごとkillされれば`onDestroy`は呼ばれず、それこそがこのマーカーで
        // 検出したい「予期しないkill」そのものになる)。
        private const val LIFECYCLE_PREFS_NAME = "isekai_terminal_service_lifecycle"
        private const val PREF_KEY_CLEAN_SHUTDOWN = "clean_shutdown"

        /**
         * 正常終了マーカーを同期的に書き込む。`commit()`を使う理由:
         * `onDestroy`直後にOSがプロセスを終了させることがあり、`apply()`の
         * 非同期書き込みが完了する前にプロセスが死ぬと書き込みが失われて
         * このマーカーの意味が無くなる(項目2の設計判断、`PLAN.md`参照)。
         */
        fun markCleanShutdown(context: Context) {
            context.getSharedPreferences(LIFECYCLE_PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putBoolean(PREF_KEY_CLEAN_SHUTDOWN, true)
                .commit()
        }

        /**
         * アプリ起動時に1回だけ呼ぶ。マーカーが存在すれば「前回プロセスは正常終了
         * だった」ことを意味するので`true`を返しつつ、直後にマーカーを消費(false相当
         * にリセット)する。消費せずに残しておくと、今回のセッションが予期せずkillされた
         * 場合でも次回起動時に2世代前の"clean"痕跡を誤って読み、予期しないkillの検出を
         * 取りこぼしてしまう(dirty-bit方式と同じ考え方)。
         */
        fun consumeCleanShutdownMarker(context: Context): Boolean {
            val prefs = context.getSharedPreferences(LIFECYCLE_PREFS_NAME, Context.MODE_PRIVATE)
            val wasClean = prefs.getBoolean(PREF_KEY_CLEAN_SHUTDOWN, false)
            prefs.edit().putBoolean(PREF_KEY_CLEAN_SHUTDOWN, false).commit()
            return wasClean
        }
    }
}
