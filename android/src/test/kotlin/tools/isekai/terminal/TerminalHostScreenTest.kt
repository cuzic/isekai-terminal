package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performSemanticsAction
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.TestCoroutineScheduler
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.session.TerminalSession
import tools.isekai.terminal.ui.TerminalThemes

/**
 * 複数タブUI([TerminalHostScreen])のタブ切り替え・クローズ・per-tab配色テーマ変更を検証する。
 * [TerminalTabsViewModelTest]と同じ[FakeOrchestrator]ベースのセットアップを使い、
 * ViewModelの状態だけでなく実際のCompose UI配線(タブクリック・×ボタン・🎨ボタン)を検証する。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalHostScreenTest {
    @get:Rule val composeTestRule = createComposeRule()

    private lateinit var vm: TerminalTabsViewModel
    private lateinit var executor: DumbAppExecutor
    private val orchestrators = mutableListOf<FakeOrchestrator>()
    private lateinit var testScheduler: TestCoroutineScheduler

    @Before
    fun setup() {
        testScheduler = TestCoroutineScheduler()
        Dispatchers.setMain(UnconfinedTestDispatcher(testScheduler))
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
        }
        executor = DumbAppExecutor()
        val sessionFactory: () -> TerminalSession = {
            val fake = FakeOrchestrator()
            orchestrators.add(fake)
            TerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> fake.also { it.callback = cb } })
        }
        vm = TerminalTabsViewModel(app, executor, sessionFactory)
    }

    @After
    fun teardown() {
        Dispatchers.resetMain()
    }

    private fun profile(label: String) = ConnectionProfile(
        label = label, host = "$label.example.com", username = "user", authType = "password",
    )

    @Test fun tabBar_rendersOneLabelPerOpenTab() {
        vm.openTab(profile("alpha"))
        vm.openTab(profile("beta"))
        composeTestRule.setContent { TerminalHostScreen(onAllTabsClosed = {}, tabsVm = vm) }
        composeTestRule.onNodeWithText("alpha").assertExists()
        composeTestRule.onNodeWithText("beta").assertExists()
    }

    @Test fun clickingInactiveTab_switchesActiveTab() {
        val idAlpha = vm.openTab(profile("alpha"))
        vm.openTab(profile("beta"))
        // openTab は開いた直後のタブ(beta)をアクティブにする。
        assertEquals(vm.tabs.value.last().tabId, vm.activeTabId.value)

        composeTestRule.setContent { TerminalHostScreen(onAllTabsClosed = {}, tabsVm = vm) }
        // ScrollableTabRow の Tab は素の performClick() だとインジケーターアニメーション絡みで
        // 座標ベースのクリックが安定しないため、既存の FilterChip テストと同じく
        // semantics の OnClick アクションを直接叩く方式にする。
        composeTestRule.onNodeWithText("alpha").performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()

        assertEquals(idAlpha, vm.activeTabId.value)
    }

    @Test fun closingOneOfTwoTabs_removesOnlyThatTabAndDisconnectsItsSession() {
        vm.openTab(profile("alpha"))
        vm.openTab(profile("beta"))
        composeTestRule.setContent { TerminalHostScreen(onAllTabsClosed = {}, tabsVm = vm) }
        assertEquals(2, vm.tabs.value.size)

        // タブ行の並び順(生成順)通りにクローズボタン(×)が並ぶはずなので、先頭(alpha)を閉じる。
        composeTestRule.onAllNodesWithText("×")[0].performClick()
        composeTestRule.waitForIdle()

        assertEquals(1, vm.tabs.value.size)
        assertEquals("beta", vm.tabs.value[0].label)
        assertTrue("閉じたタブのセッションはdisconnectされるべき", orchestrators[0].disconnectCalled)
    }

    @Test fun closingLastRemainingTab_invokesOnAllTabsClosed() {
        vm.openTab(profile("alpha"))
        var closedCallbackInvoked = false
        composeTestRule.setContent {
            TerminalHostScreen(onAllTabsClosed = { closedCallbackInvoked = true }, tabsVm = vm)
        }
        composeTestRule.onNodeWithText("×").performClick()
        composeTestRule.waitForIdle()

        assertTrue(closedCallbackInvoked)
    }

    @Test fun themeButton_opensDialog_andSelectingThemeOverridesOnlyThatTab() {
        vm.openTab(profile("alpha"))
        composeTestRule.setContent { TerminalHostScreen(onAllTabsClosed = {}, tabsVm = vm) }

        composeTestRule.onNodeWithText(TerminalThemes.DRACULA.name).assertDoesNotExist()
        composeTestRule.onNodeWithText("🎨").performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText(TerminalThemes.DRACULA.name).performClick()
        composeTestRule.waitForIdle()

        val tab = vm.tabs.value.first()
        assertTrue("テーマを個別選択したタブはoverride扱いになるべき", tab.isThemeOverridden)
        assertEquals(TerminalThemes.DRACULA.name, tab.currentTheme.value.name)
    }
}
