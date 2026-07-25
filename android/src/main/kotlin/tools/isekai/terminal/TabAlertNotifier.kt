package tools.isekai.terminal

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import uniffi.isekai_terminal_core.NotifyKind

/**
 * タスク#57: tmux hook(alert-bell/alert-activity/alert-silence/pane-died)発火を
 * Android通知として見せる。
 *
 * 「今この瞬間ユーザーへ見せるべきか」(アプリがフォアグラウンドかつこのタブが表示中なら
 * 抑制する)の判断は Rust 側(`SessionOrchestrator` の `OrchestratorAdapter::on_notify`、
 * `(tmux_tag, seq)` の重複排除もそこで完結している)が既に済ませてから
 * [TerminalSession] の `onNotify` コールバックを呼ぶため、ここで責任を持つのは
 * 次の2点だけ(`.claude/rules/rust-ssot.md`: セッション状態に基づく判断はRust側、
 * 純粋なUI opt-in設定・OS権限確認はKotlin側でよい):
 * 1. このタブ(プロファイル)自身の通知opt-in設定([ConnectionProfile.enableTabNotifications]、
 *    既定OFF)。
 * 2. `POST_NOTIFICATIONS` 実行時権限(Android 13/API 33+)が実際に付与されているか。
 *
 * 通知チャンネルは [TerminalSessionService] が使う常駐セッション通知
 * (`CHANNEL_ID = "isekai_terminal_session_main"`, IMPORTANCE_LOW)とは別の
 * チャンネルにしてある——こちらは「今すぐ気づいてほしい」一過性のアラートなので
 * IMPORTANCE_DEFAULT(音・ポップアップ)にする。
 */
object TabAlertNotifier {
    const val CHANNEL_ID = "isekai_tmux_tab_alerts"

    /** 通知IDの衝突を避けるための基準値(他の通知機能とID帯を分けるだけの単純なオフセット)。 */
    private const val NOTIFICATION_ID_BASE = 20_000

    /**
     * 通知チャンネルを作成する(Android 8.0/API 26+で必須、既に存在すれば無害な
     * no-op)。アプリ起動時に一度呼んでおけばよい(`MainActivity.onCreate`参照)。
     */
    fun createChannel(context: Context) {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "tmux通知",
            NotificationManager.IMPORTANCE_DEFAULT,
        ).apply {
            description = "tmuxセッションのベル・アクティビティ・無音・コマンド終了(pane-died)"
        }
        context.getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    /**
     * `POST_NOTIFICATIONS` が実際に付与されているか。API 32以下はこの権限自体が
     * 存在しない(マニフェスト宣言だけで常に通知可能)ため常に`true`を返す。
     */
    fun hasPermission(context: Context): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return true
        return ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
    }

    /**
     * [kind] に応じた通知タイトル・本文。プロファイル名([profileLabel])を含めることで
     * 複数タブ/複数ホストを開いている場合にどのタブの出来事か分かるようにする。
     *
     * `WAITING`/`DONE`/`INFO`(AI/汎用の注目通知)はここには来ない想定
     * (Rust側`OrchestratorAdapter::on_notify`はtmux hook由来の4種のみをこの
     * コールバック経由で流し、AI側は`TerminalSession.onNotify`という別配線
     * ——`session.rs`のマッチで両ファミリーが分岐済み——を経由するため)だが、
     * `NotifyKind`が両ファミリーを1つのenumに統合しているため`when`を網羅する
     * 必要があり、フォールバック文言を用意する。
     */
    internal fun titleAndTextFor(kind: NotifyKind, profileLabel: String): Pair<String, String> =
        when (kind) {
            NotifyKind.BELL -> "$profileLabel: ベル" to "端末がベルを鳴らしました"
            NotifyKind.ACTIVITY -> "$profileLabel: アクティビティ" to "ウィンドウに出力がありました"
            NotifyKind.SILENCE -> "$profileLabel: 無音" to "しばらく出力がありません"
            NotifyKind.JOB_DONE -> "$profileLabel: コマンド終了" to "実行中のコマンドが終了しました"
            NotifyKind.WAITING -> "$profileLabel: 入力待ち" to "リモート側が入力待ちです"
            NotifyKind.DONE -> "$profileLabel: 完了" to "リモート側の処理が完了しました"
            NotifyKind.INFO -> "$profileLabel: 通知" to "新しい通知があります"
        }

    /**
     * [enabled](プロファイルの`enableTabNotifications`)がfalse、または通知権限が
     * 無ければ何もしない(黙ったフォールバック、opportunistic機能)。[tabId]は
     * 通知IDをタブごとに分ける(同じタブの新しい通知が古い通知を上書きし、別タブの
     * 通知とは重ならないようにする)ためだけに使う——内容には出さない。
     */
    fun notify(context: Context, tabId: String, profileLabel: String, kind: NotifyKind, enabled: Boolean) {
        if (!enabled) return
        if (!hasPermission(context)) return
        val (title, text) = titleAndTextFor(kind, profileLabel)
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .build()
        // `String.hashCode()`は負値も返し得るため、そのままNOTIFICATION_ID_BASEに
        // 足すと他の通知機能(例: TerminalSessionServiceのフォアグラウンド通知
        // ID=1002)のID帯と衝突し得る(opusレビューLow指摘)。符号ビットを落として
        // 常に非負にし、かつ固定幅のID帯(NOTIFICATION_ID_BASE〜+9999)へ収める。
        val notificationId = NOTIFICATION_ID_BASE + (tabId.hashCode() and 0x7fffffff) % 10_000
        try {
            androidx.core.app.NotificationManagerCompat.from(context).notify(notificationId, notification)
        } catch (_: SecurityException) {
            // hasPermission()チェック直後でも、OS側の権限剥奪との間に稀な競合が
            // あり得る(ユーザーが設定画面から丁度取り消した直後、等)。lintの
            // MissingPermission警告を避けるためのtry/catchでもあり、単に握り潰して
            // 通知を諦めるだけでよい(opportunistic機能)。
        }
    }
}
