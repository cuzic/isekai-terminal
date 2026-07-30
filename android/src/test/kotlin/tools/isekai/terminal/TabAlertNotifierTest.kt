package tools.isekai.terminal

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import android.content.pm.PackageManager
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.isekai_terminal_core.NotifyKind

/**
 * タスク#57: [TabAlertNotifier]の通知チャンネル作成・権限確認・opt-in設定に基づく
 * 抑制ロジックのテスト。「今この瞬間見せるべきか」(フォアグラウンド+タブ表示中の抑制、
 * `(tmux_tag, seq)`重複排除)はRust側([rust-core] `OrchestratorAdapter::on_notify`)の
 * 責務でありここでは扱わない——ここで検証するのは(a) プロファイル単位opt-in、
 * (b) 通知権限、の2つのゲートと、通知チャンネル自体が正しく作られることだけ。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class TabAlertNotifierTest {
    private lateinit var app: Application

    @Before
    fun setup() {
        app = ApplicationProvider.getApplicationContext()
    }

    @Test
    fun createChannel_registersChannelWithExpectedId() {
        TabAlertNotifier.createChannel(app)

        val manager = app.getSystemService(NotificationManager::class.java)
        val channel = manager.getNotificationChannel(TabAlertNotifier.CHANNEL_ID)
        assertEquals(TabAlertNotifier.CHANNEL_ID, channel?.id)
    }

    @Test
    fun hasPermission_falseWhenNotGranted() {
        shadowOf(app).denyPermissions(Manifest.permission.POST_NOTIFICATIONS)
        assertFalse(TabAlertNotifier.hasPermission(app))
    }

    @Test
    fun hasPermission_trueWhenGranted() {
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        assertTrue(TabAlertNotifier.hasPermission(app))
    }

    @Test
    fun notify_doesNothingWhenDisabledEvenWithPermission() {
        TabAlertNotifier.createChannel(app)
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)

        TabAlertNotifier.notify(app, tabId = "1", profileLabel = "myhost", kind = NotifyKind.BELL, enabled = false)

        val manager = app.getSystemService(NotificationManager::class.java)
        assertTrue(shadowOf(manager).allNotifications.isEmpty())
    }

    @Test
    fun notify_doesNothingWhenEnabledButPermissionMissing() {
        TabAlertNotifier.createChannel(app)
        shadowOf(app).denyPermissions(Manifest.permission.POST_NOTIFICATIONS)

        TabAlertNotifier.notify(app, tabId = "1", profileLabel = "myhost", kind = NotifyKind.BELL, enabled = true)

        val manager = app.getSystemService(NotificationManager::class.java)
        assertTrue(shadowOf(manager).allNotifications.isEmpty())
    }

    @Test
    fun notify_postsWhenEnabledAndPermissionGranted() {
        TabAlertNotifier.createChannel(app)
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)

        TabAlertNotifier.notify(app, tabId = "1", profileLabel = "myhost", kind = NotifyKind.BELL, enabled = true)

        val manager = app.getSystemService(NotificationManager::class.java)
        assertEquals(1, shadowOf(manager).allNotifications.size)
    }

    @Test
    fun notify_distinctTabIdsUseDistinctNotificationSlots() {
        TabAlertNotifier.createChannel(app)
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)

        TabAlertNotifier.notify(app, tabId = "tab-a", profileLabel = "host-a", kind = NotifyKind.BELL, enabled = true)
        TabAlertNotifier.notify(app, tabId = "tab-b", profileLabel = "host-b", kind = NotifyKind.ACTIVITY, enabled = true)

        val manager = app.getSystemService(NotificationManager::class.java)
        assertEquals(2, shadowOf(manager).allNotifications.size)
    }

    @Test
    fun notify_usesProvidedMessageInsteadOfDefaultTextWhenPresent() {
        TabAlertNotifier.createChannel(app)
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)

        TabAlertNotifier.notify(
            app,
            tabId = "1",
            profileLabel = "myhost",
            kind = NotifyKind.WAITING,
            enabled = true,
            message = "Claude Code" to "needs your input",
        )

        val manager = app.getSystemService(NotificationManager::class.java)
        val extras = shadowOf(manager).allNotifications.single().extras
        assertEquals("myhost: Claude Code", extras.getCharSequence(android.app.Notification.EXTRA_TITLE).toString())
        assertEquals("needs your input", extras.getCharSequence(android.app.Notification.EXTRA_TEXT).toString())
    }

    @Test
    fun notify_fallsBackToDefaultTextWhenMessagePartsAreBlank() {
        TabAlertNotifier.createChannel(app)
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)

        TabAlertNotifier.notify(
            app,
            tabId = "1",
            profileLabel = "myhost",
            kind = NotifyKind.WAITING,
            enabled = true,
            message = "" to "",
        )

        val manager = app.getSystemService(NotificationManager::class.java)
        val extras = shadowOf(manager).allNotifications.single().extras
        val (defaultTitle, defaultText) = TabAlertNotifier.titleAndTextFor(NotifyKind.WAITING, "myhost")
        assertEquals(defaultTitle, extras.getCharSequence(android.app.Notification.EXTRA_TITLE).toString())
        assertEquals(defaultText, extras.getCharSequence(android.app.Notification.EXTRA_TEXT).toString())
    }

    @Test
    fun titleAndTextFor_producesDistinctTextPerKind() {
        val texts = NotifyKind.values().map { TabAlertNotifier.titleAndTextFor(it, "myhost") }
        assertEquals(texts.size, texts.toSet().size)
        texts.forEach { (title, _) -> assertTrue(title.contains("myhost")) }
    }

    /** API 32以下は`POST_NOTIFICATIONS`権限自体が存在せず、常に許可扱いになる。 */
    @Config(sdk = [28])
    @Test
    fun hasPermission_alwaysTrueBelowApi33() {
        assertTrue(TabAlertNotifier.hasPermission(app))
    }
}
