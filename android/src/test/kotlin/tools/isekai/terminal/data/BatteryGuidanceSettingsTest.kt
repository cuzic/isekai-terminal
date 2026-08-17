package tools.isekai.terminal.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class BatteryGuidanceSettingsTest {

    private val context get() = RuntimeEnvironment.getApplication()

    @Test
    fun unexpectedKillCount_defaultsToZero() {
        assertEquals(0, BatteryGuidanceSettings.unexpectedKillCount(context))
    }

    @Test
    fun incrementUnexpectedKillCount_incrementsAndPersists() {
        assertEquals(1, BatteryGuidanceSettings.incrementUnexpectedKillCount(context))
        assertEquals(2, BatteryGuidanceSettings.incrementUnexpectedKillCount(context))
        assertEquals(2, BatteryGuidanceSettings.unexpectedKillCount(context))
    }

    @Test
    fun lastShownUnixSecs_defaultsToNull() {
        assertNull(BatteryGuidanceSettings.lastShownUnixSecs(context))
    }

    @Test
    fun markShownNow_persistsTimestamp() {
        BatteryGuidanceSettings.markShownNow(context, 12345L)
        assertEquals(12345L, BatteryGuidanceSettings.lastShownUnixSecs(context))
    }

    @Test
    fun optedOut_defaultsToFalseAndCanBeToggled() {
        assertFalse(BatteryGuidanceSettings.isOptedOut(context))
        BatteryGuidanceSettings.setOptedOut(context, true)
        assertTrue(BatteryGuidanceSettings.isOptedOut(context))
        BatteryGuidanceSettings.setOptedOut(context, false)
        assertFalse(BatteryGuidanceSettings.isOptedOut(context))
    }
}
