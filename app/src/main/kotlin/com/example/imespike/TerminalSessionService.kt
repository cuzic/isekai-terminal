package com.example.imespike

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.Binder
import android.os.IBinder
import uniffi.tssh_core.SshSessionInterface

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

    // SshSessionInterface の GC root（Service がプロセスに存在する限り GC されない）
    private var sshSession: SshSessionInterface? = null

    fun holdSession(s: SshSessionInterface) { sshSession = s }
    fun releaseSession() { sshSession = null }
    fun getSession(): SshSessionInterface? = sshSession

    fun notifyConnected(host: String) {
        updateNotification("接続中: $host")
    }
    fun notifyDisconnected() {
        updateNotification("切断済み")
    }

    override fun onBind(intent: Intent): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val label = intent?.getStringExtra(EXTRA_SESSION_LABEL) ?: "SSH セッション"
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
    }

    // ── 通知 ──────────────────────────────────────────────

    private fun startForegroundWithNotification(label: String) {
        val notification = buildNotification(label)
        // Android 14+: foregroundServiceType は Manifest で宣言（remoteMessaging）
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
        private const val CHANNEL_ID = "tssh_session_main"
        private const val NOTIFICATION_ID = 1002
    }
}
