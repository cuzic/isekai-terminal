package tools.isekai.terminal.util

import android.app.Application
import android.os.PowerManager
import android.provider.Settings
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class BatteryOptimizationTest {

    @Test
    fun isIgnoringBatteryOptimizations_reflectsShadowState() {
        val context = RuntimeEnvironment.getApplication()
        val powerManager = context.getSystemService(PowerManager::class.java)
        val shadowPowerManager = shadowOf(powerManager)

        assertFalse(BatteryOptimization.isIgnoringBatteryOptimizations(context))

        shadowPowerManager.setIsIgnoringBatteryOptimizations(true)

        assertTrue(BatteryOptimization.isIgnoringBatteryOptimizations(context))
    }

    @Test
    fun openIgnoreBatteryOptimizationSettings_startsSettingsListIntent() {
        val context = RuntimeEnvironment.getApplication()
        val shadowApp = shadowOf(context as Application)

        BatteryOptimization.openIgnoreBatteryOptimizationSettings(context)

        val started = shadowApp.nextStartedActivity
        assertEquals(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS, started?.action)
    }
}
