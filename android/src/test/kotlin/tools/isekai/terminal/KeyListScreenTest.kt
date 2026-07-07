package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.KeyEntry
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
class KeyListScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun clearKeys() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(ctx)
        runBlocking { Repositories.keys.getAll().forEach { Repositories.keys.delete(it) } }
    }

    private fun insertKey(label: String) = runBlocking {
        Repositories.keys.save(
            KeyEntry(
                label = label,
                publicKey = "ssh-ed25519 AAAAC3$label",
                encryptedPrivateKeyPath = "/keys/$label.enc",
                kekAlias = "kek_$label",
                createdAt = 1_700_000_000_000L,
            )
        )
    }

    private fun waitForText(text: String) {
        composeTestRule.waitUntil(5000) {
            composeTestRule.onAllNodesWithText(text, substring = true).fetchSemanticsNodes().isNotEmpty()
        }
    }

    @Test fun emptyState_showsImportPrompt() {
        composeTestRule.setContent { KeyListScreen(onImportKey = {}, onBack = {}) }
        waitForText("「＋」でインポート")
        composeTestRule.onNodeWithText("「＋」でインポート、「生成」で新規作成", substring = true).assertExists()
    }

    @Test fun keyWithLabel_isDisplayed() {
        insertKey("My SSH Key")
        composeTestRule.setContent { KeyListScreen(onImportKey = {}, onBack = {}) }
        waitForText("My SSH Key")
        composeTestRule.onNodeWithText("My SSH Key").assertExists()
    }

    @Test fun fabClick_callsOnImportKey() {
        var imported = false
        composeTestRule.setContent { KeyListScreen(onImportKey = { imported = true }, onBack = {}) }
        // "＋" FAB is in Scaffold slot, no scroll needed
        composeTestRule.onNodeWithText("＋").performClick()
        composeTestRule.waitForIdle()
        assertTrue(imported)
    }

    @Test fun backButton_callsOnBack() {
        var backed = false
        composeTestRule.setContent { KeyListScreen(onImportKey = {}, onBack = { backed = true }) }
        // "戻る" is in the top Row of the main Column, no scroll needed
        composeTestRule.onNodeWithText("戻る").performClick()
        composeTestRule.waitForIdle()
        assertTrue(backed)
    }

    @Test fun deleteButton_showsConfirmDialog() {
        insertKey("My SSH Key")
        composeTestRule.setContent { KeyListScreen(onImportKey = {}, onBack = {}) }
        waitForText("削除")
        composeTestRule.onNodeWithText("削除").performScrollTo().performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("削除しますか", substring = true).assertExists()
    }
}
