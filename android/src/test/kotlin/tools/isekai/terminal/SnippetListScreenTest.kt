package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class SnippetListScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun clearDb() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(ctx)
        runBlocking { Repositories.snippets.getAll().forEach { Repositories.snippets.delete(it) } }
    }

    private fun insertSnippet(snippet: Snippet) = runBlocking { Repositories.snippets.save(snippet) }

    private fun setScreen(
        onAddSnippet: () -> Unit = {},
        onEditSnippet: (Snippet) -> Unit = {},
        onBack: () -> Unit = {},
    ) {
        composeTestRule.setContent {
            SnippetListScreen(onAddSnippet = onAddSnippet, onEditSnippet = onEditSnippet, onBack = onBack)
        }
        composeTestRule.waitForIdle()
    }

    @Test fun emptyList_showsPlaceholder() {
        setScreen()
        composeTestRule.onNodeWithText("「＋」をタップして定型コマンドを追加").assertExists()
    }

    @Test fun withSnippets_rendersLabelsAndFirstCommandLine() {
        insertSnippet(Snippet(label = "list files", command = "ls -la\necho done"))
        setScreen()
        composeTestRule.onNodeWithText("list files").assertExists()
        composeTestRule.onNodeWithText("ls -la").assertExists()
    }

    @Test fun snippetWithoutProfileId_showsCommonLabel() {
        insertSnippet(Snippet(label = "common", command = "uptime", profileId = null))
        setScreen()
        composeTestRule.onNodeWithText("全プロファイル共通").assertExists()
    }

    @Test fun snippetWithProfileId_showsProfileSpecificLabel() {
        insertSnippet(Snippet(label = "specific", command = "uptime", profileId = 42L))
        setScreen()
        composeTestRule.onNodeWithText("特定プロファイル専用").assertExists()
    }

    @Test fun clickingCard_invokesOnEditSnippetWithThatSnippet() {
        insertSnippet(Snippet(label = "list files", command = "ls -la"))
        var edited: Snippet? = null
        setScreen(onEditSnippet = { edited = it })

        composeTestRule.onNodeWithText("list files").performClick()
        composeTestRule.waitForIdle()

        assertEquals("list files", edited?.label)
    }

    @Test fun editButton_invokesOnEditSnippet() {
        insertSnippet(Snippet(label = "list files", command = "ls -la"))
        var edited: Snippet? = null
        setScreen(onEditSnippet = { edited = it })

        composeTestRule.onNodeWithText("編集").performClick()
        composeTestRule.waitForIdle()

        assertEquals("list files", edited?.label)
    }

    @Test fun deleteButton_showsConfirmationDialog_withoutDeletingYet() {
        insertSnippet(Snippet(label = "list files", command = "ls -la"))
        setScreen()
        composeTestRule.onNodeWithText("削除").performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("削除確認").assertExists()
        composeTestRule.onNodeWithText("「list files」を削除しますか？").assertExists()
        runBlocking { assertEquals(1, Repositories.snippets.getAll().size) }
    }

    @Test fun deleteConfirmation_dismiss_keepsSnippet() {
        insertSnippet(Snippet(label = "list files", command = "ls -la"))
        setScreen()
        composeTestRule.onNodeWithText("削除").performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("キャンセル").performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("削除確認").assertDoesNotExist()
        runBlocking { assertEquals(1, Repositories.snippets.getAll().size) }
    }

    @Test fun deleteConfirmation_confirm_deletesSnippet() {
        insertSnippet(Snippet(label = "list files", command = "ls -la"))
        setScreen()
        // 「削除」ボタンは各カードの削除ボタンと、確認ダイアログ内の確定ボタンとで
        // ラベルが同じため(x2)、確認ダイアログを開いた後は onAllNodesWithText の
        // 最後の要素(ダイアログの確定ボタン)をクリックする。
        composeTestRule.onNodeWithText("削除").performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onAllNodesWithText("削除").assertCountEquals(2)
        composeTestRule.onAllNodesWithText("削除")[1].performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("削除確認").assertDoesNotExist()
        runBlocking { assertTrue(Repositories.snippets.getAll().isEmpty()) }
    }

    @Test fun addButton_invokesOnAddSnippet() {
        var addClicked = false
        setScreen(onAddSnippet = { addClicked = true })
        composeTestRule.onNodeWithText("＋").performClick()
        composeTestRule.waitForIdle()
        assertTrue(addClicked)
    }

    @Test fun backButton_invokesOnBack() {
        var backClicked = false
        setScreen(onBack = { backClicked = true })
        composeTestRule.onNodeWithText("戻る").performClick()
        composeTestRule.waitForIdle()
        assertTrue(backClicked)
    }
}
