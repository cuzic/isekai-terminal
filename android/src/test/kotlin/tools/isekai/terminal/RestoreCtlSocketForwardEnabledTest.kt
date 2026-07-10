package tools.isekai.terminal

import android.app.Application
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test
import org.robolectric.RobolectricTestRunner
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * [MainActivity.restorePersistedCtlSocketForward]の中核ロジック
 * [restoreCtlSocketForwardEnabled] のユニットテスト。
 * MainActivity起動時の一度きりの復元処理はprivateかつnative
 * ([uniffi.isekai_terminal_core.setCtlSocketForwardEnabled])を直接呼んでいたため
 * JVMテストで検証できなかった。実行中のトグル([ProfileListScreen]のメニュー)は
 * [ProfileListScreenTest] 側で別途検証済み。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class RestoreCtlSocketForwardEnabledTest {
    private lateinit var prefs: android.content.SharedPreferences

    @Before fun setup() {
        val app = ApplicationProvider.getApplicationContext<Application>()
        prefs = app.getSharedPreferences("isekai_terminal_ui", Context.MODE_PRIVATE)
        prefs.edit().clear().apply()
    }

    @Test fun defaultsToFalse_whenNoPreferenceIsPersisted() {
        var applied: Boolean? = null

        restoreCtlSocketForwardEnabled(prefs, apply = { applied = it })

        assertEquals(false, applied)
    }

    @Test fun appliesPersistedTrueValue() {
        prefs.edit().putBoolean(PREF_KEY_ENABLE_CTL_SOCKET_FORWARD, true).apply()
        var applied: Boolean? = null

        restoreCtlSocketForwardEnabled(prefs, apply = { applied = it })

        assertEquals(true, applied)
    }

    @Test fun appliesPersistedFalseValue() {
        prefs.edit().putBoolean(PREF_KEY_ENABLE_CTL_SOCKET_FORWARD, false).apply()
        var applied: Boolean? = null

        restoreCtlSocketForwardEnabled(prefs, apply = { applied = it })

        assertEquals(false, applied)
    }
}
