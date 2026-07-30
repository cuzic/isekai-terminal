package tools.isekai.terminal

import android.app.Service
import android.content.Intent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * 2026-07-27に実機で確認したクラッシュループ(プロセスkill後、STARTSTICKYによる
 * OSの自動再起動→中身の無いforeground通知→Android 15+のFGS制限で強制終了→
 * 2回目の自動再起動がmAllowStartForeground=falseで即クラッシュ、を無限に繰り返す)
 * の回帰防止テスト。[TerminalSessionService.onStartCommand]がnull Intent
 * (=START_STICKYの仕様でOSが再送する、元のIntentを持たない再起動呼び出し)を
 * 受けたときに、startForeground()せず即座に自分を停止することを検証する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class TerminalSessionServiceTest {

    @Test
    fun onStartCommand_withNullIntent_stopsSelfWithoutStartingForeground() {
        val controller = Robolectric.buildService(TerminalSessionService::class.java).create()
        val service = controller.get()

        val result = service.onStartCommand(null, 0, 1)

        assertEquals(Service.START_NOT_STICKY, result)
        assertTrue(shadowOf(service).isStoppedBySelf)
        assertNull(shadowOf(service).lastForegroundNotification)
    }

    @Test
    fun onStartCommand_withRealIntent_startsForegroundAndStaysSticky() {
        val controller = Robolectric.buildService(TerminalSessionService::class.java).create()
        val service = controller.get()
        val intent = Intent(service, TerminalSessionService::class.java)
            .putExtra(TerminalSessionService.EXTRA_SESSION_LABEL, "cuzic@example.com")

        val result = service.onStartCommand(intent, 0, 1)

        assertEquals(Service.START_STICKY, result)
        assertTrue(!shadowOf(service).isStoppedBySelf)
        assertEquals(
            "cuzic@example.com",
            shadowOf(service).lastForegroundNotification?.extras?.getCharSequence(android.app.Notification.EXTRA_TEXT).toString(),
        )
    }
}
