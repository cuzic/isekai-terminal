package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import kotlinx.coroutines.runBlocking
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
    }

    private fun insertProfile(profile: ConnectionProfile) = runBlocking { Repositories.profiles.save(profile) }

    private fun setScreen(
        onConnect: (ConnectionProfile, String?) -> Unit = { _, _ -> },
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
        // "鍵管理" is in topBar Row, no scroll needed
        composeTestRule.onNodeWithText("鍵管理").performClick()
        composeTestRule.waitUntil(3000) { managed }
        assertTrue(managed)
    }
}
