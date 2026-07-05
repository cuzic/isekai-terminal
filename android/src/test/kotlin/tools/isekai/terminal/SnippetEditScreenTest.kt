package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTextInput
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class SnippetEditScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun clearDb() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(ctx)
        runBlocking {
            Repositories.snippets.getAll().forEach { Repositories.snippets.delete(it) }
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
        }
    }

    @Test fun newSnippet_showsAddTitle() {
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("定型コマンド追加").assertExists()
    }

    @Test fun editSnippet_showsEditTitle_andPrefillsFields() {
        val snippet = Snippet(label = "list files", command = "ls -la", appendNewline = false)
        composeTestRule.setContent { SnippetEditScreen(snippet = snippet, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("定型コマンド編集").assertExists()
        composeTestRule.onNodeWithText("list files").assertExists()
        composeTestRule.onNodeWithText("ls -la").assertExists()
    }

    @Test fun saveButton_disabledInitially() {
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()
    }

    @Test fun saveButton_enabledAfterFillingLabelAndCommand() {
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = {}, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("list files")
        fields[1].performTextInput("ls -la")
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsEnabled()
    }

    @Test fun cancelButton_invokesOnCancel() {
        var cancelled = false
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = {}, onCancel = { cancelled = true }) }
        composeTestRule.onNodeWithText("キャンセル").performScrollTo().performClick()
        composeTestRule.waitForIdle()
        assertTrue(cancelled)
    }

    @Test fun savingNewSnippet_persistsLabelCommandAndDefaultAppendNewline() {
        var saved = false
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = { saved = true }, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("list files")
        fields[1].performTextInput("ls -la")
        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.snippets.getAll().first { it.label == "list files" }
            assertEquals("ls -la", stored.command)
            assertTrue(stored.appendNewline)
            assertNull(stored.profileId)
        }
    }

    @Test fun togglingAppendNewlineSwitch_andSaving_persistsFalse() {
        var saved = false
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = { saved = true }, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("no newline")
        fields[1].performTextInput("cat file")
        composeTestRule.onNodeWithText("末尾で Enter する").assertExists()
        composeTestRule.onNodeWithTag("appendNewlineSwitch").performClick()
        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.snippets.getAll().first { it.label == "no newline" }
            assertTrue("appendNewlineはOFFにしたのでfalseで保存されるべき", !stored.appendNewline)
        }
    }

    @Test fun profileDropdown_defaultsToCommonAcrossAllProfiles() {
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("全プロファイル共通").assertExists()
    }

    @Test fun selectingSpecificProfile_andSaving_persistsProfileId() {
        val profileId = runBlocking {
            Repositories.profiles.save(
                ConnectionProfile(label = "Prod", host = "prod.example.com", username = "deploy", authType = "password"),
            )
        }
        var saved = false
        composeTestRule.setContent { SnippetEditScreen(snippet = null, onSave = { saved = true }, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("prod only")
        fields[1].performTextInput("systemctl restart app")

        composeTestRule.onNodeWithText("全プロファイル共通").performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("Prod").performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.snippets.getAll().first { it.label == "prod only" }
            assertEquals(profileId, stored.profileId)
        }
    }

    @Test fun editingSnippet_prefillsAssignedProfile() {
        val profileId = runBlocking {
            Repositories.profiles.save(
                ConnectionProfile(label = "Prod", host = "prod.example.com", username = "deploy", authType = "password"),
            )
        }
        val snippet = Snippet(label = "prod only", command = "uptime", profileId = profileId)
        composeTestRule.setContent { SnippetEditScreen(snippet = snippet, onSave = {}, onCancel = {}) }
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("Prod").assertExists()
    }
}
