package tools.isekai.terminal

import android.view.WindowManager
import androidx.activity.ComponentActivity
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * 画面の保護(FLAG_SECURE, #62)の中核ロジック [applyScreenProtection] のユニットテスト。
 * Compose UI 経由のトグル(メニュー項目 → SharedPreferences 永続化)は
 * [ProfileListScreenTest] 側で検証する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class ScreenProtectionTest {

    @Test fun enabled_setsFlagSecure() {
        val activity = Robolectric.buildActivity(ComponentActivity::class.java).setup().get()

        applyScreenProtection(activity, enabled = true)

        val flags = activity.window.attributes.flags
        assertEquals(
            WindowManager.LayoutParams.FLAG_SECURE,
            flags and WindowManager.LayoutParams.FLAG_SECURE,
        )
    }

    @Test fun disabled_clearsFlagSecure() {
        val activity = Robolectric.buildActivity(ComponentActivity::class.java).setup().get()
        applyScreenProtection(activity, enabled = true)

        applyScreenProtection(activity, enabled = false)

        val flags = activity.window.attributes.flags
        assertEquals(0, flags and WindowManager.LayoutParams.FLAG_SECURE)
    }
}
