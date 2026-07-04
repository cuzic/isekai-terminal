package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertIsOn
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTextInput
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.data.Repositories
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
class ProfileEditScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    @Before fun setup() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(ctx)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.keys.getAll().forEach { Repositories.keys.delete(it) }
        }
    }

    private fun sampleProfile() = tools.isekai.terminal.data.ConnectionProfile(
        label = "Prod", host = "prod.example.com", port = 2222,
        username = "deploy", authType = "password",
    )

    private fun insertKey(label: String) = runBlocking {
        Repositories.keys.save(
            KeyEntry(
                label = label,
                publicKey = "ssh-ed25519 AAAAC3$label",
                encryptedPrivateKeyPath = "/keys/$label.enc",
                kekAlias = "kek_$label",
                createdAt = 1_700_000_000_000L,
            ),
        )
    }

    @Test fun newProfile_showsAddTitle() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("プロファイル追加").assertExists()
    }

    @Test fun editProfile_showsEditTitle() {
        composeTestRule.setContent { ProfileEditScreen(profile = sampleProfile(), onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("プロファイル編集").assertExists()
    }

    @Test fun editProfile_prefillsFields() {
        composeTestRule.setContent { ProfileEditScreen(profile = sampleProfile(), onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("Prod").assertExists()
        composeTestRule.onNodeWithText("prod.example.com").assertExists()
        composeTestRule.onNodeWithText("deploy").assertExists()
    }

    @Test fun saveButton_disabledInitially() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()
    }

    @Test fun saveButton_enabledAfterFilling() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("My Server")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsEnabled()
    }

    @Test fun cancelButton_callsOnCancel() {
        var cancelled = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = { cancelled = true })
        }
        composeTestRule.onNodeWithText("キャンセル").performScrollTo().performClick()
        composeTestRule.waitForIdle()
        assertTrue(cancelled)
    }

    @Test fun authChip_key_showsKeyDropdown() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
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
        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        assertTrue(saved)
        runBlocking { assertTrue(Repositories.profiles.getAll().any { it.label == "Bastion" }) }
    }

    // ── SSH agent forwarding トグル ─────────────────────────────────────

    @Test fun agentForwardToggle_hiddenWarning_untilEnabled() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("SSH agent forwarding").assertExists()
        composeTestRule.onNodeWithText("信頼できるホストのみで有効にしてください", substring = true).assertDoesNotExist()
    }

    @Test fun agentForwardToggle_enabling_showsWarning() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("SSH agent forwarding").performScrollTo()
        composeTestRule.onNodeWithTag("agentForwardSwitch").performScrollTo().performClick()
        composeTestRule.onNodeWithText("信頼できるホストのみで有効にしてください", substring = true).assertExists()
    }

    @Test fun editProfile_prefillsAgentForwardEnabled() {
        val profile = sampleProfile().copy(enableAgentForward = true)
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
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
        composeTestRule.onNodeWithTag("agentForwardSwitch").performScrollTo().performClick()
        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "Bastion2" }
            assertTrue(stored.enableAgentForward)
        }
    }

    // ── 踏み台(ProxyJump) ───────────────────────────────────────────────

    @Test fun jumpHostToggle_hiddenFields_untilEnabled() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("踏み台(ProxyJump)経由で接続する").assertExists()
        composeTestRule.onNodeWithText("踏み台ホスト").assertDoesNotExist()
    }

    @Test fun jumpHostToggle_enabling_showsFields() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithTag("useJumpHostCheckbox").performScrollTo().performClick()
        composeTestRule.onNodeWithText("踏み台ホスト").assertExists()
        composeTestRule.onNodeWithText("踏み台ポート").assertExists()
        composeTestRule.onNodeWithText("踏み台ユーザー名").assertExists()
    }

    @Test fun saveButton_disabledWhenJumpHostEnabledButIncomplete() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("WithJump")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsEnabled()

        composeTestRule.onNodeWithTag("useJumpHostCheckbox").performScrollTo().performClick()
        // 踏み台のホスト/ユーザー名が未入力の間は保存不可であるべき。
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()
    }

    @Test fun saveNewProfile_withJumpHost_persistsJumpFields() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("ViaBastion")
        fields[1].performTextInput("internal.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithTag("useJumpHostCheckbox").performScrollTo().performClick()
        composeTestRule.onNodeWithText("踏み台ホスト").performTextInput("bastion.example.com")
        composeTestRule.onNodeWithText("踏み台ユーザー名").performTextInput("jumper")
        // 踏み台の認証方式は既定でパスワードなので鍵選択は不要、これで保存可能なはず。

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "ViaBastion" }
            assertTrue(stored.usesJumpHost)
            assertTrue(stored.jumpHost == "bastion.example.com")
            assertTrue(stored.jumpUsername == "jumper")
            assertTrue(stored.jumpAuthType == "password")
        }
    }

    @Test fun editProfile_prefillsJumpHostFields() {
        val profile = sampleProfile().copy(
            jumpHost = "bastion.example.com",
            jumpPort = 2200,
            jumpUsername = "jumper",
            jumpAuthType = "password",
        )
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithTag("useJumpHostCheckbox").assertIsOn()
        composeTestRule.onNodeWithText("bastion.example.com").assertExists()
        composeTestRule.onNodeWithText("jumper").assertExists()
        composeTestRule.onNodeWithText("2200").assertExists()
    }

    @Test fun saveNewProfile_withJumpHostKeyAuth_persistsJumpKeyId() {
        val jumpKeyId = insertKey("BastionKey")
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("ViaBastionKey")
        fields[1].performTextInput("internal.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithTag("useJumpHostCheckbox").performScrollTo().performClick()
        composeTestRule.onNodeWithText("踏み台ホスト").performTextInput("bastion.example.com")
        composeTestRule.onNodeWithText("踏み台ユーザー名").performTextInput("jumper")

        // 「鍵認証」チップは主ホスト用/踏み台用の2つ存在するため、2番目(踏み台側)をクリックする。
        composeTestRule.onAllNodesWithText("鍵認証")[1].performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("踏み台の鍵").performScrollTo().performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("BastionKey").performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "ViaBastionKey" }
            assertEquals("key", stored.jumpAuthType)
            assertEquals(jumpKeyId, stored.jumpKeyId)
        }
    }

    @Test fun editProfile_prefillsJumpHostKeyAuth() {
        val jumpKeyId = insertKey("BastionKey")
        val profile = sampleProfile().copy(
            jumpHost = "bastion.example.com",
            jumpUsername = "jumper",
            jumpAuthType = "key",
            jumpKeyId = jumpKeyId,
        )
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("BastionKey").assertExists()
    }

    // ── STUN+SSHランデブー方式のP2P ─────────────────────────────────────

    @Test fun stunChip_hiddenField_untilSelected() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("STUN P2P QUIC（実験的）").assertExists()
        composeTestRule.onNodeWithText("STUNサーバー（任意）").assertDoesNotExist()
    }

    @Test fun stunChip_selecting_showsStunServerField() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("STUN P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("STUNサーバー（任意）").assertExists()
    }

    @Test fun stunServerField_isOptional_saveButtonStillEnabled() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("StunHost")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("STUN P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        // stun_server は任意入力なので、未入力のままでも保存可能であるべき。
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsEnabled()
    }

    @Test fun saveNewProfile_withStunServer_persistsField() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("StunProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("STUN P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("STUNサーバー（任意）").performTextInput("stun.example.com:3478")
        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "StunProfile" }
            assertTrue(stored.transportPreferenceName == "ISEKAI_STUN_P2P_QUIC")
            assertTrue(stored.stunServer == "stun.example.com:3478")
        }
    }

    @Test fun editProfile_prefillsStunServer() {
        val profile = sampleProfile().copy(
            transportPreferenceName = "ISEKAI_STUN_P2P_QUIC",
            stunServer = "stun.example.com:3478",
        )
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("stun.example.com:3478").assertExists()
    }

    // ── MASQUE relay経由のP2P ───────────────────────────────────────────

    @Test fun relayChip_hiddenFields_untilSelected() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("relay P2P QUIC（実験的）").assertExists()
        composeTestRule.onNodeWithText("relayアドレス（host:port）").assertDoesNotExist()
    }

    @Test fun relayChip_selecting_showsRelayFields() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("relay P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("relayアドレス（host:port）").assertExists()
        composeTestRule.onNodeWithText("relay SNI").assertExists()
        composeTestRule.onNodeWithText("relay JWT").assertExists()
    }

    @Test fun saveButton_disabledWhenRelaySelectedButIncomplete() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("WithRelay")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsEnabled()

        composeTestRule.onNodeWithText("relay P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        // relayアドレス/SNI/JWTの3つが揃うまでは保存不可であるべき。
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()

        composeTestRule.onNodeWithText("relayアドレス（host:port）").performTextInput("relay.example.com:443")
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()

        composeTestRule.onNodeWithText("relay SNI").performTextInput("relay.example.com")
        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()
    }

    @Test fun saveNewProfile_withRelayConfig_persistsAllThreeFields() {
        var saved = false
        composeTestRule.setContent {
            // relayJwtの暗号化(RelayCredentialVault)はAndroidKeyStoreを使うためRobolectricでは
            // 動かない。ここでは恒等関数に差し替えて配線ロジックだけを検証する。
            ProfileEditScreen(
                profile = null,
                onSave = { saved = true },
                onCancel = {},
                encryptRelayJwt = { it },
                decryptRelayJwt = { it },
            )
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("RelayProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithText("relay P2P QUIC（実験的）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("relayアドレス（host:port）").performTextInput("relay.example.com:443")
        composeTestRule.onNodeWithText("relay SNI").performTextInput("relay.example.com")
        composeTestRule.onNodeWithText("relay JWT").performTextInput("eyJhbGciOiJSUzI1NiJ9.test.sig")

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "RelayProfile" }
            assertTrue(stored.transportPreferenceName == "ISEKAI_LINK_RELAY_QUIC")
            assertTrue(stored.relayAddr == "relay.example.com:443")
            assertTrue(stored.relaySni == "relay.example.com")
            assertTrue(stored.relayJwt == "eyJhbGciOiJSUzI1NiJ9.test.sig")
            assertTrue(stored.hasRelayConfig)
        }
    }

    @Test fun editProfile_prefillsRelayFields() {
        val profile = sampleProfile().copy(
            transportPreferenceName = "ISEKAI_LINK_RELAY_QUIC",
            relayAddr = "relay.example.com:443",
            relaySni = "relay.example.com",
            relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
        )
        composeTestRule.setContent {
            ProfileEditScreen(
                profile = profile,
                onSave = {},
                onCancel = {},
                encryptRelayJwt = { it },
                decryptRelayJwt = { it },
            )
        }
        composeTestRule.onNodeWithText("relay.example.com:443").assertExists()
        composeTestRule.onNodeWithText("relay.example.com").assertExists()
        composeTestRule.onNodeWithText("eyJhbGciOiJSUzI1NiJ9.test.sig").assertExists()
    }

    // ── Phase 12 P2-1: per-session/per-hostのterminal theme ──────────────

    @Test fun newProfile_defaultsToFollowingGlobalTheme() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("既定に従う").assertIsSelected()
    }

    @Test fun selectingProfileTheme_andSaving_persistsThemeName() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("ThemedProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithText(tools.isekai.terminal.ui.TerminalThemes.DRACULA.name)
            .performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText(tools.isekai.terminal.ui.TerminalThemes.DRACULA.name).assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "ThemedProfile" }
            assertTrue(stored.themeName == tools.isekai.terminal.ui.TerminalThemes.DRACULA.name)
        }
    }

    @Test fun editProfile_prefillsSelectedTheme() {
        val profile = sampleProfile().copy(themeName = tools.isekai.terminal.ui.TerminalThemes.NORD.name)
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText(tools.isekai.terminal.ui.TerminalThemes.NORD.name).assertIsSelected()
    }

    // ── Phase 12 P2-2: Remote/Dynamic port forward ───────────────────────

    @Test fun addingRemoteForward_andSaving_persistsRemoteForward() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        var fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("WithRemoteForward")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithText("+ ポートフォワードを追加").performScrollTo().performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("Remote (-R)").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("Remote (-R)").assertIsSelected()

        // フォワード追加後の新規テキストフィールド: [4]=接続後コマンド, [5]=待受アドレス
        // (プリフィル済み), [6]=待受ポート, [7]=ローカルターゲットホスト, [8]=ローカルターゲットポート。
        fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[6].performTextInput("8080")
        fields[7].performTextInput("192.168.1.5")
        fields[8].performTextInput("9090")
        composeTestRule.onNodeWithText("Remote (-R)").assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "WithRemoteForward" }
            assertEquals(1, stored.forwards.size)
            val fw = stored.forwards[0]
            assertEquals(uniffi.tssh_core.ForwardType.REMOTE, fw.forwardType)
            assertEquals(8080.toUShort(), fw.bindPort)
            assertEquals("192.168.1.5", fw.remoteHost)
            assertEquals(9090.toUShort(), fw.remotePort)
        }
    }

    @Test fun addingDynamicForward_andSaving_persistsDynamicForwardWithoutTarget() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        var fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("WithSocksForward")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithText("+ ポートフォワードを追加").performScrollTo().performClick()
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("Dynamic/SOCKS (-D)").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()

        // Dynamicは転送先ホスト/ポート欄が表示されないため、待受ポートのみ入力する。
        // [4]=接続後コマンド, [5]=待受アドレス(プリフィル済み), [6]=待受ポート。
        fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[6].performTextInput("1080")

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "WithSocksForward" }
            assertEquals(1, stored.forwards.size)
            val fw = stored.forwards[0]
            assertEquals(uniffi.tssh_core.ForwardType.DYNAMIC, fw.forwardType)
            assertEquals(1080.toUShort(), fw.bindPort)
        }
    }

    @Test fun editProfile_prefillsRemoteForward() {
        val profile = sampleProfile().copy(
            forwards = listOf(
                uniffi.tssh_core.PortForward(
                    forwardType = uniffi.tssh_core.ForwardType.REMOTE,
                    bindAddress = "0.0.0.0", bindPort = 8080u,
                    remoteHost = "192.168.1.5", remotePort = 9090u,
                ),
            ),
        )
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("Remote (-R)").assertIsSelected()
        composeTestRule.onNodeWithText("0.0.0.0").assertExists()
        composeTestRule.onNodeWithText("8080").assertExists()
        composeTestRule.onNodeWithText("192.168.1.5").assertExists()
        composeTestRule.onNodeWithText("9090").assertExists()
    }

    @Test fun editProfile_prefillsDynamicForward() {
        val profile = sampleProfile().copy(
            forwards = listOf(
                uniffi.tssh_core.PortForward(
                    forwardType = uniffi.tssh_core.ForwardType.DYNAMIC,
                    bindAddress = "127.0.0.1", bindPort = 1080u,
                    remoteHost = "", remotePort = 0u,
                ),
            ),
        )
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("Dynamic/SOCKS (-D)").assertIsSelected()
        composeTestRule.onNodeWithText("1080").assertExists()
        // Dynamicは転送先ホスト/ポート欄が表示されないはず。
        composeTestRule.onNodeWithText("転送先ホスト").assertDoesNotExist()
    }

    // ── 非ループバックbind許可 ─────────────────────────────────────────

    @Test fun allowNonLoopbackForwardBindCheckbox_enabling_andSaving_persistsFlag() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("NonLoopbackHost")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")

        composeTestRule.onNodeWithTag("allowNonLoopbackForwardBindCheckbox").performScrollTo().performClick()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "NonLoopbackHost" }
            assertTrue(stored.allowNonLoopbackForwardBind)
        }
    }

    @Test fun editProfile_prefillsAllowNonLoopbackForwardBind() {
        val profile = sampleProfile().copy(allowNonLoopbackForwardBind = true)
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithTag("allowNonLoopbackForwardBindCheckbox").assertIsOn()
    }

    // ── トランスポート選択(TransportPreference) ───────────────────────────
    // STUN P2P / relay P2P は上のセクションで既にチップ選択+保存を検証済みなので、
    // ここでは残りの全チップ(既定のPLAIN_SSH含む)を網羅する。

    @Test fun savingWithDefaultTransportChip_persistsPlainSsh() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("DefaultTransport")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("通常 SSH").assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "DefaultTransport" }
            assertEquals("PLAIN_SSH", stored.transportPreferenceName)
        }
    }

    @Test fun selectingTsshdQuicChip_andSaving_persistsTransportPreference() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("TsshdProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("tsshd QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("tsshd QUIC").assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "TsshdProfile" }
            assertEquals("TSSHD_QUIC", stored.transportPreferenceName)
        }
    }

    @Test fun selectingAutoChip_andSaving_persistsTransportPreference() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("AutoProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("Auto（推奨）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("Auto（推奨）").assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "AutoProfile" }
            assertEquals("AUTO", stored.transportPreferenceName)
        }
    }

    @Test fun selectingHelperQuicChip_andSaving_persistsTransportPreference() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("HelperQuicProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "HelperQuicProfile" }
            assertEquals("ISEKAI_HELPER_QUIC", stored.transportPreferenceName)
        }
    }

    // ── 自作ヘルパーQUICの待受ポート固定 ─────────────────────────────────

    @Test fun helperBindPortField_hiddenForPlainSsh_shownForHelperQuicChips() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").assertDoesNotExist()

        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").assertExists()
    }

    @Test fun selectingHelperQuic_settingBindPort_andSaving_persistsHelperBindPort() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("FixedPortProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").performScrollTo().performTextInput("45900")

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "FixedPortProfile" }
            assertEquals(45900, stored.helperBindPort)
        }
    }

    @Test fun helperBindPort_leftBlank_savesAsNull() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("EphemeralPortProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "EphemeralPortProfile" }
            assertEquals(null, stored.helperBindPort)
        }
    }

    @Test fun helperBindPort_belowPrivilegedRange_disablesSave() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("BadPortProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").performScrollTo().performTextInput("80")

        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()
    }

    @Test fun helperBindPort_abovePortRange_disablesSave() {
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = {}, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("TooHighPortProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        // 5桁までしか入力を許していないため、65536以上は現状の桁数制限では防げない値として
        // 99999(5桁の最大値、範囲外)を使う。
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").performScrollTo().performTextInput("99999")

        composeTestRule.onNodeWithText("保存").performScrollTo().assertIsNotEnabled()
    }

    @Test fun helperBindPort_atRangeBoundaries_allowsSave() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("BoundaryPortProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").performScrollTo().performTextInput("1024")

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "BoundaryPortProfile" }
            assertEquals(1024, stored.helperBindPort)
        }
    }

    @Test fun editProfile_prefillsHelperBindPort() {
        val profile = sampleProfile().copy(
            transportPreferenceName = "ISEKAI_HELPER_QUIC",
            helperBindPort = 45900,
        )
        composeTestRule.setContent { ProfileEditScreen(profile = profile, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("45900").assertExists()
    }

    @Test fun selectingHelperQuicMultipathChip_andSaving_persistsTransportPreference() {
        var saved = false
        composeTestRule.setContent {
            ProfileEditScreen(profile = null, onSave = { saved = true }, onCancel = {})
        }
        val fields = composeTestRule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("HelperQuicMultipathProfile")
        fields[1].performTextInput("host.example.com")
        fields[3].performTextInput("root")
        composeTestRule.onNodeWithText("自作ヘルパー QUIC（マルチパス）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("自作ヘルパー QUIC（マルチパス）").assertIsSelected()

        composeTestRule.onNodeWithText("保存").performScrollTo().performClick()
        composeTestRule.waitUntil(5000) {
            composeTestRule.waitForIdle()
            saved
        }
        runBlocking {
            val stored = Repositories.profiles.getAll().first { it.label == "HelperQuicMultipathProfile" }
            assertEquals("ISEKAI_HELPER_QUIC_MULTIPATH", stored.transportPreferenceName)
        }
    }

    @Test fun multipathWithDirectAddress_andBlankBindPort_showsDefaultPortNote() {
        composeTestRule.setContent { ProfileEditScreen(profile = null, onSave = {}, onCancel = {}) }
        composeTestRule.onNodeWithText("自作ヘルパー QUIC（マルチパス）").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("既定の固定ポート45823が使われます", substring = true).assertDoesNotExist()

        composeTestRule.onNodeWithText("直接到達アドレス（path1、任意）").performScrollTo().performTextInput("203.0.113.5:45823")
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("既定の固定ポート45823が使われます", substring = true).assertExists()

        // 固定ポートを明示指定すれば、既定値ノートは消える。
        composeTestRule.onNodeWithText("ヘルパー待受ポート固定（任意、1024〜65535）").performScrollTo().performTextInput("50000")
        composeTestRule.onNodeWithText("既定の固定ポート45823が使われます", substring = true).assertDoesNotExist()
    }
}
