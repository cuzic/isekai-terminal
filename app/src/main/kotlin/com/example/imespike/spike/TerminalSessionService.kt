package com.example.imespike.spike

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.Binder
import android.os.IBinder
import com.example.imespike.MainActivity
import com.example.imespike.util.RemoteLogger

/**
 * ターミナルセッションを保持する Foreground Service。
 *
 * - Activity が破棄（画面回転・バックグラウンド移行）されてもセッションを継続する
 * - Android 14 以降では foregroundServiceType の宣言が必須
 * - P0 スパイク: サービスが起動し通知が出ることを確認する
 */
class TerminalSessionService : Service() {

    inner class SessionBinder : Binder() {
        fun getService(): TerminalSessionService = this@TerminalSessionService
    }

    private val binder = SessionBinder()
    private var sessionLabel: String = "接続なし"

    override fun onBind(intent: Intent): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        RemoteLogger.i("TsshSvc", "service created")
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val label = intent?.getStringExtra(EXTRA_SESSION_LABEL) ?: "SSH セッション"
        RemoteLogger.i("TsshSvc", "onStartCommand label='$label' flags=$flags startId=$startId")
        startForegroundWithNotification(label)
        return START_STICKY
    }

    fun updateNotification(label: String) {
        sessionLabel = label
        val manager = getSystemService(NotificationManager::class.java)
        manager.notify(NOTIFICATION_ID, buildNotification(label))
    }

    override fun onDestroy() {
        super.onDestroy()
        RemoteLogger.i("TsshSvc", "service destroyed")
    }

    // ── 通知 ──────────────────────────────────────────────

    private fun startForegroundWithNotification(label: String) {
        val notification = buildNotification(label)
        // Android 14+: foregroundServiceType は Manifest で宣言（connectedDevice）
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
            .setContentTitle("android-tssh")
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
        private const val CHANNEL_ID = "tssh_session"
        private const val NOTIFICATION_ID = 1001
    }
}
