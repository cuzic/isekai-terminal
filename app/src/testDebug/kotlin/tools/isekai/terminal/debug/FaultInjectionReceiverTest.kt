package tools.isekai.terminal.debug

import android.content.Context
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Phase 8-4: 実機なしで `FaultInjectionReceiver` の intent 解釈ロジックを検証する。
 * native FFI (`uniffi.tssh_core.debug*`) は実機/エミュレータでしか動かないため、
 * [FaultInjectorApi] を fake に差し替えて、5つの broadcast action が正しい
 * 引数で正しい呼び出しにマッピングされることだけを Robolectric 上で確認する。
 * `scripts/phase7-5-roaming-test.sh` が実機で送る `adb shell am broadcast` の
 * action/extra 名（`ms`, `permille`）と一致していることの回帰チェックを兼ねる。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class FaultInjectionReceiverTest {

    private class FakeFaultInjectorApi : FaultInjectorApi {
        val calls = mutableListOf<String>()
        override fun setLatencyMs(ms: UInt) {
            calls += "setLatencyMs($ms)"
        }
        override fun setLossPermille(permille: UInt) {
            calls += "setLossPermille($permille)"
        }
        override fun cut() {
            calls += "cut()"
        }
        override fun restore() {
            calls += "restore()"
        }
        override fun clear() {
            calls += "clear()"
        }
    }

    private lateinit var context: Context
    private lateinit var receiver: FaultInjectionReceiver
    private lateinit var fake: FakeFaultInjectorApi

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        fake = FakeFaultInjectorApi()
        receiver = FaultInjectionReceiver().apply { faultInjector = fake }
    }

    @Test
    fun setLatency_parsesMsExtraAndCallsInjector() {
        val intent = Intent("tools.isekai.terminal.debug.SET_LATENCY").putExtra("ms", 300)
        receiver.onReceive(context, intent)
        assertEquals(listOf("setLatencyMs(300)"), fake.calls)
    }

    @Test
    fun setLatency_missingExtraDefaultsToZero() {
        val intent = Intent("tools.isekai.terminal.debug.SET_LATENCY")
        receiver.onReceive(context, intent)
        assertEquals(listOf("setLatencyMs(0)"), fake.calls)
    }

    @Test
    fun setLoss_parsesPermilleExtraAndCallsInjector() {
        val intent = Intent("tools.isekai.terminal.debug.SET_LOSS").putExtra("permille", 200)
        receiver.onReceive(context, intent)
        assertEquals(listOf("setLossPermille(200)"), fake.calls)
    }

    @Test
    fun cut_callsInjectorWithNoArgs() {
        receiver.onReceive(context, Intent("tools.isekai.terminal.debug.CUT"))
        assertEquals(listOf("cut()"), fake.calls)
    }

    @Test
    fun restore_callsInjectorWithNoArgs() {
        receiver.onReceive(context, Intent("tools.isekai.terminal.debug.RESTORE"))
        assertEquals(listOf("restore()"), fake.calls)
    }

    @Test
    fun clear_callsInjectorWithNoArgs() {
        receiver.onReceive(context, Intent("tools.isekai.terminal.debug.CLEAR"))
        assertEquals(listOf("clear()"), fake.calls)
    }

    @Test
    fun unknownAction_doesNothing() {
        receiver.onReceive(context, Intent("tools.isekai.terminal.debug.NOT_A_REAL_ACTION"))
        assertTrue("unknown action must not call the injector", fake.calls.isEmpty())
    }

    @Test
    fun nullAction_doesNothing() {
        receiver.onReceive(context, Intent())
        assertTrue("null action must not call the injector", fake.calls.isEmpty())
    }
}
