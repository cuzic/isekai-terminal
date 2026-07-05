package tools.isekai.terminal

import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
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
class ProfileEditScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun setup() {
        val ctx = InstrumentationRegistry.getInstrumentation().targetContext
        Repositories.init(ctx)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.keys.getAll().forEach { Repositories.keys.delete(it) }
        }
    }

    private fun sampleProfile() = ConnectionProfile(
        label = "Prod",
        host = "prod.example.com",
        port = 2222,
        username = "deploy",
        authType = "password",
    )

    @Test fun newProfile_showsAddTitle() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("プロファイル追加").assertExists()
    }

    @Test fun editProfile_showsEditTitle() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = sampleProfile(), onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("プロファイル編集").assertExists()
    }

    @Test fun editProfile_prefillsFields() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = sampleProfile(), onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("Prod").assertExists()
        composeTestRule.onNodeWithText("prod.example.com").assertExists()
        composeTestRule.onNodeWithText("deploy").assertExists()
    }

    @Test fun saveButton_disabledInitially() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("保存").assertIsNotEnabled()
    }

    @Test fun saveButton_enabledAfterFilling() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("My Server")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("保存").assertIsEnabled()
    }

    @Test fun cancelButton_callsOnCancel() {
        var cancelled = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = { cancelled = true })
        }
        composeTestRule.onNodeWithText("キャンセル").performClick()
        composeTestRule.waitForIdle()
        assertTrue(cancelled)
    }

    @Test fun authChip_key_showsKeyDropdown() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("鍵認証").performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("鍵を選択").assertExists()
    }

    @Test fun saveNewProfile_callsOnSave() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("Bastion")
        fields[1].performTextInput("bastion.example.com")
        fields[3].performTextInput("admin")
        composeTestRule.onNodeWithText("保存").performClick()
        composeTestRule.waitUntil(timeoutMillis = 5000) { saved }
        assertTrue(saved)
        runBlocking {
            assertTrue(Repositories.profiles.getAll().any { it.label == "Bastion" })
        }
    }

    // ── SSH agent forwarding トグル ─────────────────────────────────────

    @Test fun agentForwardToggle_hiddenWarning_untilEnabled() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("SSH agent forwarding").assertExists()
        composeTestRule.onNodeWithText("信頼できるホストのみで有効にしてください", substring = true).assertDoesNotExist()
    }

    @Test fun agentForwardToggle_enabling_showsWarning() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithTag("agentForwardSwitch").performClick()
        composeTestRule.onNodeWithText("信頼できるホストのみで有効にしてください", substring = true).assertExists()
    }

    @Test fun saveNewProfile_withAgentForwardEnabled_persistsFlag() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("Bastion2")
        fields[1].performTextInput("bastion2.example.com")
        fields[3].performTextInput("admin")
        composeTestRule.onNodeWithTag("agentForwardSwitch").performClick()
        composeTestRule.onNodeWithText("保存").performClick()
        composeTestRule.waitUntil(timeoutMillis = 5000) { saved }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "Bastion2" }
            assertTrue(stored.enableAgentForward)
        }
    }
}
