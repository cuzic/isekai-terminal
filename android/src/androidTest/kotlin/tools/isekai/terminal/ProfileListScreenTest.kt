package tools.isekai.terminal

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ProfileListScreenTest {
    @get:Rule
    val composeTestRule = createComposeRule()

    @Before
    fun clearDb() {
        val ctx = InstrumentationRegistry.getInstrumentation().targetContext
        Repositories.init(ctx)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
        }
    }

    private fun insertProfile(profile: ConnectionProfile) = runBlocking {
        Repositories.profiles.save(profile)
    }

    private fun setScreen(
        onConnect: (ConnectionProfile, String?, String?) -> Unit = { _, _, _ -> },
        onAddProfile: () -> Unit = {},
        onEditProfile: (ConnectionProfile) -> Unit = {},
        onManageKeys: () -> Unit = {},
    ) {
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = onConnect,
                onAddProfile = onAddProfile,
                onEditProfile = onEditProfile,
                onManageKeys = onManageKeys,
            )
        }
        composeTestRule.waitForIdle()
    }

    private fun waitForText(text: String) {
        composeTestRule.waitUntil(3000) {
            composeTestRule.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty()
        }
    }

    @Test
    fun emptyState_showsAddPrompt() {
        setScreen()
        waitForText("「＋」をタップして接続先を追加")
        composeTestRule.onNodeWithText("「＋」をタップして接続先を追加").assertIsDisplayed()
    }

    @Test
    fun profileWithLabel_isDisplayed() {
        insertProfile(
            ConnectionProfile(
                label = "My Server", host = "host", username = "user", authType = "password",
            ),
        )
        setScreen()
        waitForText("My Server")
        composeTestRule.onNodeWithText("My Server").assertIsDisplayed()
    }

    @Test
    fun passwordProfile_tap_showsPasswordDialog() {
        insertProfile(
            ConnectionProfile(
                label = "PwHost", host = "host", username = "user", authType = "password",
            ),
        )
        setScreen()
        waitForText("PwHost")
        composeTestRule.onNodeWithText("PwHost").performClick()
        waitForText("パスワード入力")
        composeTestRule.onNodeWithText("パスワード入力").assertIsDisplayed()
        composeTestRule.onNodeWithText("接続").assertIsDisplayed()
    }

    @Test
    fun keyProfile_tap_callsOnConnectDirectly() {
        insertProfile(
            ConnectionProfile(
                label = "KeyHost", host = "host", username = "user",
                authType = "key", keyId = 1L,
            ),
        )
        var connected = false
        setScreen(onConnect = { _, _, _ -> connected = true })
        waitForText("KeyHost")
        composeTestRule.onNodeWithText("KeyHost").performClick()
        composeTestRule.waitUntil(3000) { connected }
        assertTrue(connected)
    }

    @Test
    fun fabClick_callsOnAddProfile() {
        var added = false
        setScreen(onAddProfile = { added = true })
        composeTestRule.onNodeWithText("＋").performClick()
        composeTestRule.waitUntil(3000) { added }
        assertTrue(added)
    }

    @Test
    fun deleteButton_showsConfirmDialog() {
        insertProfile(
            ConnectionProfile(
                label = "DelHost", host = "host", username = "user", authType = "password",
            ),
        )
        setScreen()
        waitForText("DelHost")
        composeTestRule.onNodeWithText("削除").performClick()
        waitForText("削除確認")
        composeTestRule.onNodeWithText("削除確認").assertIsDisplayed()
    }

    @Test
    fun manageKeysButton_callsCallback() {
        var managed = false
        setScreen(onManageKeys = { managed = true })
        composeTestRule.onNodeWithContentDescription("メニュー").performClick()
        composeTestRule.onNodeWithText("鍵管理").performClick()
        composeTestRule.waitUntil(3000) { managed }
        assertTrue(managed)
    }
}
