package com.example.imespike

import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class KeyImportScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Test fun initialState_showsTitle() {
        composeTestRule.setContent {
            KeyImportScreen(onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("秘密鍵をインポート").assertExists()
    }

    @Test fun initialState_saveButtonEnabled() {
        composeTestRule.setContent {
            KeyImportScreen(onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("保存").assertIsEnabled()
    }

    @Test fun saveClick_withoutFile_showsError() {
        composeTestRule.setContent {
            KeyImportScreen(onSave = {}, onCancel = {})
        }
        composeTestRule.onNodeWithText("保存").performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("PEM ファイルを選択してください").assertExists()
    }

    @Test fun cancelButton_callsOnCancel() {
        var cancelled = false
        composeTestRule.setContent {
            KeyImportScreen(onSave = {}, onCancel = { cancelled = true })
        }
        composeTestRule.onNodeWithText("キャンセル").performClick()
        composeTestRule.waitForIdle()
        assertTrue(cancelled)
    }

    @Test fun labelField_acceptsInput() {
        composeTestRule.setContent {
            KeyImportScreen(onSave = {}, onCancel = {})
        }
        composeTestRule.onAllNodes(hasSetTextAction())[0].performTextInput("my-key")
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("my-key").assertExists()
    }
}
