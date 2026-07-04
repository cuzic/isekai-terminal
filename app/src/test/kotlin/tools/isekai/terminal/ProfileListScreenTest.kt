package tools.isekai.terminal

import android.app.Application
import android.content.Context
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.ui.TerminalThemes
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class ProfileListScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun clearDb() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(ctx)
        runBlocking { Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) } }
        // 配色テーマの永続化テストが互いに影響しないよう毎回クリアする
        ctx.getSharedPreferences("tssh_ui", Context.MODE_PRIVATE).edit().clear().apply()
    }

    private fun insertProfile(profile: ConnectionProfile) = runBlocking { Repositories.profiles.save(profile) }

    private fun setScreen(
        onConnect: (ConnectionProfile, String?) -> Unit = { _, _ -> },
        onAddProfile: () -> Unit = {},
        onEditProfile: (ConnectionProfile) -> Unit = {},
        onManageKeys: () -> Unit = {},
        // Rust への実反映(native)はテストでは呼びたくないので既定で no-op に差し替える
        applyTerminalTheme: (TerminalTheme) -> Unit = {},
    ) {
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = onConnect,
                onAddProfile = onAddProfile,
                onEditProfile = onEditProfile,
                onManageKeys = onManageKeys,
                applyTerminalTheme = applyTerminalTheme,
            )
        }
        composeTestRule.waitForIdle()
    }

    private fun waitForText(text: String) {
        composeTestRule.waitUntil(3000) {
            composeTestRule.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty()
        }
    }

    @Test fun emptyState_showsAddPrompt() {
        setScreen()
        waitForText("「＋」をタップして接続先を追加")
        composeTestRule.onNodeWithText("「＋」をタップして接続先を追加").assertIsDisplayed()
    }

    @Test fun profileWithLabel_isDisplayed() {
        insertProfile(ConnectionProfile(label = "My Server", host = "host", username = "user", authType = "password"))
        setScreen()
        waitForText("My Server")
        composeTestRule.onNodeWithText("My Server").assertIsDisplayed()
    }

    @Test fun passwordProfile_tap_showsPasswordDialog() {
        insertProfile(ConnectionProfile(label = "PwHost", host = "host", username = "user", authType = "password"))
        setScreen()
        waitForText("PwHost")
        composeTestRule.onNodeWithText("PwHost").performScrollTo().performClick()
        waitForText("パスワード入力")
        composeTestRule.onNodeWithText("パスワード入力").assertIsDisplayed()
        composeTestRule.onNodeWithText("接続").assertIsDisplayed()
    }

    @Test fun keyProfile_tap_callsOnConnectDirectly() {
        insertProfile(ConnectionProfile(label = "KeyHost", host = "host", username = "user", authType = "key", keyId = 1L))
        var connected = false
        setScreen(onConnect = { _, _ -> connected = true })
        waitForText("KeyHost")
        composeTestRule.onNodeWithText("KeyHost").performScrollTo().performClick()
        composeTestRule.waitUntil(3000) { connected }
        assertTrue(connected)
    }

    @Test fun fabClick_callsOnAddProfile() {
        var added = false
        setScreen(onAddProfile = { added = true })
        // FAB is in Scaffold slot, no scroll needed
        composeTestRule.onNodeWithText("＋").performClick()
        composeTestRule.waitUntil(3000) { added }
        assertTrue(added)
    }

    @Test fun deleteButton_showsConfirmDialog() {
        insertProfile(ConnectionProfile(label = "DelHost", host = "host", username = "user", authType = "password"))
        setScreen()
        waitForText("DelHost")
        composeTestRule.onNodeWithText("削除").performScrollTo().performClick()
        waitForText("削除確認")
        composeTestRule.onNodeWithText("削除確認").assertIsDisplayed()
    }

    @Test fun manageKeysButton_callsCallback() {
        var managed = false
        setScreen(onManageKeys = { managed = true })
        // "鍵管理" はハンバーガーメニュー内にある
        composeTestRule.onNodeWithContentDescription("メニュー").performClick()
        composeTestRule.onNodeWithText("鍵管理").performClick()
        composeTestRule.waitUntil(3000) { managed }
        assertTrue(managed)
    }

    // ── 配色テーマ選択（案C）────────────────────────────────────────────

    @Test fun themeButton_opensThemeDialog() {
        setScreen()
        composeTestRule.onNodeWithContentDescription("メニュー").performClick()
        composeTestRule.onNodeWithText("配色").performClick()
        waitForText("配色テーマ")
        composeTestRule.onNodeWithText("配色テーマ").assertIsDisplayed()
        // 全プリセットがラジオリストとして表示される
        TerminalThemes.ALL.forEach { theme ->
            composeTestRule.onNodeWithText(theme.name).assertIsDisplayed()
        }
    }

    @Test fun selectingTheme_persistsToPrefsAndAppliesToRust() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        var appliedTheme: TerminalTheme? = null
        setScreen(applyTerminalTheme = { appliedTheme = it })

        composeTestRule.onNodeWithContentDescription("メニュー").performClick()
        composeTestRule.onNodeWithText("配色").performClick()
        waitForText("配色テーマ")
        composeTestRule.onNodeWithText(TerminalThemes.DRACULA.name).performClick()

        // 選択したテーマが (native 呼び出しの代わりに注入した) applyTerminalTheme に渡る
        assertEquals(TerminalThemes.DRACULA, appliedTheme)

        // SharedPreferences("tssh_ui") にプリセット名として永続化される
        val prefs = ctx.getSharedPreferences("tssh_ui", Context.MODE_PRIVATE)
        assertEquals(TerminalThemes.DRACULA.name, prefs.getString(TerminalThemes.PREF_KEY, null))

        // 選択後はダイアログが閉じる
        composeTestRule.onAllNodesWithText("配色テーマ").assertCountEquals(0)
    }

    @Test fun themeDialog_dismissWithoutSelection_leavesPrefsUntouched() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        setScreen()
        composeTestRule.onNodeWithContentDescription("メニュー").performClick()
        composeTestRule.onNodeWithText("配色").performClick()
        waitForText("配色テーマ")
        composeTestRule.onNodeWithText("閉じる").performClick()

        val prefs = ctx.getSharedPreferences("tssh_ui", Context.MODE_PRIVATE)
        assertEquals(null, prefs.getString(TerminalThemes.PREF_KEY, null))
    }
}
