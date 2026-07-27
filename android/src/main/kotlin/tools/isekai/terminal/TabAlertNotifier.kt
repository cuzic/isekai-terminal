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
 * タスク#57(tmux hook: alert-bell/alert-activity/alert-silence/pane-died)と
 * `AI_INTEGRATION_DESIGN.md` §6.1(AI/汎用の注目通知: Waiting/Done/Info、
 * 2026-07-25にAndroid側の配線を追加)の両方の`Notify`をAndroid通知として見せる、
 * 共通の通知経路。
 *
 * tmux hook系kindは「今この瞬間ユーザーへ見せるべきか」(アプリがフォアグラウンドかつ
 * このタブが表示中なら抑制する)の判断をRust側(`SessionOrchestrator`の
 * `OrchestratorAdapter::on_notify`、`(tmux_tag, seq)`の重複排除もそこで完結している)が
 * 済ませてから[TerminalSession]の`onNotifyRequested`コールバックを呼ぶ。一方AI系kind
 * (`TerminalSession`の`onNotify`経由、`notify_generation`ベース)には同種の
 * フォアグラウンド/タブフォーカス抑制がまだ無い(2026-07-25時点、既知の差異——
 * `AI_INTEGRATION_DESIGN.md` §11.1.4参照)ため、当該タブを見ている最中でも通知が
 * 出うる。ここで責任を持つのは次の2点だけ(`.claude/rules/rust-ssot.md`: セッション
 * 状態に基づく判断はRust側、純粋なUI opt-in設定・OS権限確認はKotlin側でよい):
 * 1. このタブ(プロファイル)自身の通知opt-in設定([ConnectionProfile.enableTabNotifications]、
 *    既定OFF)。
 * 2. `POST_NOTIFICATIONS` 実行時権限(Android 13/API 33+)が実際に付与されているか。
 *
 * 通知チャンネルは [TerminalSessionService] が使う常駐セッション通知
 * (`CHANNEL_ID = "isekai_terminal_session_main"`, IMPORTANCE_LOW)とは別の
 * チャンネルにしてある——こちらは「今すぐ気づいてほしい」一過性のアラートなので
 * IMPORTANCE_DEFAULT(音・ポップアップ)にする。「状態ドット」(タブバーの永続的な
 * 視覚インジケータ)・通知タップでの該当タブへのジャンプは、いずれもまだ実装していない
 * (このオブジェクトが提供するのは一過性のシステム通知のみ)。
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
            "タブ通知",
            NotificationManager.IMPORTANCE_DEFAULT,
        ).apply {
            description = "tmuxセッションのベル・アクティビティ・無音・コマンド終了(pane-died)、" +
                "およびAI/汎用の注目通知(入力待ち・完了)"
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
     * [kind] に応じた通知タイトル・本文の既定値(呼び出し元が[notify]に[message]を
     * 渡さなかった場合のフォールバック)。プロファイル名([profileLabel])を含めることで
     * 複数タブ/複数ホストを開いている場合にどのタブの出来事か分かるようにする。
     *
     * `WAITING`/`DONE`/`INFO`(AI/汎用の注目通知)は実際には送出側(`isekai-pipe ctl
     * notify`/claude-hookd)が持つtitle/bodyがあるため、[notify]は通常こちらの
     * フォールバックではなく[message]経由の実際の文言を使う(2026-07-25配線)。
     * `NotifyKind`がtmux hook系(tag/seqのみでtitle/body自体を持たない)とAI系を
     * 1つのenumに統合しているため`when`は両方を網羅する必要がある。
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
     *
     * [message]は`Waiting`/`Done`/`Info`(AI/汎用の注目通知、`AI_INTEGRATION_DESIGN.md`
     * §6.1)のように送出側が実際のtitle/bodyを持っている場合に渡す——`isekai-pipe ctl
     * notify <title> <body>`はCLI引数で任意の文言を送れる汎用コマンドであり、
     * [titleAndTextFor]の固定文言で上書きしてしまうと送出側の意図が失われるため。
     * `null`(既定、tmux hook系kindはtag/seqのみでtitle/bodyを持たない)の場合は
     * これまで通り[titleAndTextFor]の固定文言にフォールバックする。
     */
    fun notify(
        context: Context,
        tabId: String,
        profileLabel: String,
        kind: NotifyKind,
        enabled: Boolean,
        message: Pair<String, String>? = null,
    ) {
        if (!enabled) return
        if (!hasPermission(context)) return
        val (defaultTitle, defaultText) = titleAndTextFor(kind, profileLabel)
        val title = message?.first?.takeIf { it.isNotBlank() }?.let { "$profileLabel: $it" } ?: defaultTitle
        val text = message?.second?.takeIf { it.isNotBlank() } ?: defaultText
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
