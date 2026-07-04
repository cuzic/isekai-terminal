package tools.isekai.terminal

import android.app.Application
import android.content.Context
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performSemanticsAction
import androidx.test.core.app.ApplicationProvider
import com.github.takahirom.roborazzi.captureRoboImage
import kotlinx.coroutines.runBlocking
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.data.Repositories

/**
 * GitHub wiki 掲載用のスクリーンショットを撮影する専用テスト。
 * 通常のテストと違いアサーションは持たず、各画面の代表的な状態を
 * app/build/outputs/roborazzi/ 配下にPNGとして保存するためだけに存在する。
 *
 * 撮影(record)するには:
 *   ./gradlew testDebugUnitTest --tests "*ScreenshotGalleryTest*" -Proborazzi.test.record=true
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class ScreenshotGalleryTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun setup() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(ctx)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.keys.getAll().forEach { Repositories.keys.delete(it) }
        }
        ctx.getSharedPreferences("tssh_ui", Context.MODE_PRIVATE).edit().clear().apply()
    }

    private fun insertProfile(profile: ConnectionProfile) = runBlocking { Repositories.profiles.save(profile) }

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

    // ── プロファイル一覧 ─────────────────────────────────────────────

    @Test fun profileList_empty() {
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = { _, _, _ -> },
                onAddProfile = {},
                onEditProfile = {},
                onManageKeys = {},
                applyTerminalTheme = {},
            )
        }
        composeTestRule.waitForIdle()
        composeTestRule.onRoot().captureRoboImage("profile_list_empty.png")
    }

    @Test fun profileList_withProfiles() {
        insertProfile(ConnectionProfile(label = "本番サーバー", host = "prod.example.com", username = "deploy", authType = "password"))
        insertProfile(ConnectionProfile(label = "開発サーバー", host = "dev.example.com", username = "dev", authType = "key", keyId = 1L))
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = { _, _, _ -> },
                onAddProfile = {},
                onEditProfile = {},
                onManageKeys = {},
                applyTerminalTheme = {},
            )
        }
        waitForText("本番サーバー")
        composeTestRule.onRoot().captureRoboImage("profile_list_with_profiles.png")
    }

    @Test fun profileList_passwordDialog() {
        insertProfile(ConnectionProfile(label = "本番サーバー", host = "prod.example.com", username = "deploy", authType = "password"))
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = { _, _, _ -> },
                onAddProfile = {},
                onEditProfile = {},
                onManageKeys = {},
                applyTerminalTheme = {},
            )
        }
        waitForText("本番サーバー")
        composeTestRule.onNodeWithText("本番サーバー").performScrollTo().performClick()
        waitForText("パスワード入力")
        composeTestRule.onRoot().captureRoboImage("profile_list_password_dialog.png")
    }

    @Test fun profileList_deleteConfirmDialog() {
        insertProfile(ConnectionProfile(label = "削除対象サーバー", host = "host", username = "user", authType = "password"))
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = { _, _, _ -> },
                onAddProfile = {},
                onEditProfile = {},
                onManageKeys = {},
                applyTerminalTheme = {},
            )
        }
        waitForText("削除対象サーバー")
        composeTestRule.onNodeWithText("削除").performScrollTo().performClick()
        waitForText("削除確認")
        composeTestRule.onRoot().captureRoboImage("profile_list_delete_confirm_dialog.png")
    }

    @Test fun profileList_themeDialog() {
        composeTestRule.setContent {
            ProfileListScreen(
                onConnect = { _, _, _ -> },
                onAddProfile = {},
                onEditProfile = {},
                onManageKeys = {},
                applyTerminalTheme = {},
            )
        }
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithContentDescription("メニュー").performClick()
        composeTestRule.onNodeWithText("配色").performClick()
        waitForText("配色テーマ")
        composeTestRule.onRoot().captureRoboImage("profile_list_theme_dialog.png")
    }

    // ── プロファイル編集 ─────────────────────────────────────────────

    @Test fun profileEdit_new() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.waitForIdle()
        composeTestRule.onRoot().captureRoboImage("profile_edit_new.png")
    }

    @Test fun profileEdit_jumpHost() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithTag("useJumpHostCheckbox").performScrollTo().performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("踏み台ホスト").performScrollTo()
        composeTestRule.waitForIdle()
        composeTestRule.onRoot().captureRoboImage("profile_edit_jump_host.png")
    }

    @Test fun profileEdit_relay() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("relay P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("relayアドレス（host:port）").performScrollTo()
        composeTestRule.waitForIdle()
        composeTestRule.onRoot().captureRoboImage("profile_edit_relay.png")
    }

    // ── 鍵管理 ───────────────────────────────────────────────────────

    @Test fun keyList_empty() {
        composeTestRule.setContent { KeyListScreen(onImportKey = {}, onBack = {}) }
        composeTestRule.waitForIdle()
        composeTestRule.onRoot().captureRoboImage("key_list_empty.png")
    }

    @Test fun keyList_withKeys() {
        insertKey("My SSH Key")
        composeTestRule.setContent { KeyListScreen(onImportKey = {}, onBack = {}) }
        waitForText("My SSH Key")
        composeTestRule.onRoot().captureRoboImage("key_list_with_keys.png")
    }

    @Test fun keyImport_initial() {
        composeTestRule.setContent { KeyImportScreen(onSave = {}, onCancel = {}) }
        composeTestRule.waitForIdle()
        composeTestRule.onRoot().captureRoboImage("key_import_initial.png")
    }
}
