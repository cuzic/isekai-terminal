package tools.isekai.terminal

import android.app.Application
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.click
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeDown
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.TestCoroutineScheduler
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.RebindFdSource
import tools.isekai.terminal.session.TerminalSession
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.PanelKind
import uniffi.isekai_terminal_core.RebindPublicState
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * `docs/adr/0020-unwired-callback-detection.md` Phase 1(§3(a′)-6、rev2 R2-1)の配線契約テスト(Compose側)。
 *
 * [WiringContract]で`UiAction`に分類した`SessionOrchestrator`の全メソッドを、本物の
 * [TerminalHostScreen]+[TerminalTabsViewModel]+[TerminalSession]を描画し、意味木から
 * (`performSemanticsAction(OnClick)`・テキスト入力・タップ)発火させて、Rust側を差し替えた
 * 記録Proxy([RecordingOrchestrator])に届くことをassertする。セットアップは[TerminalHostScreenTest]と同じ。
 * 本番の`TerminalScreenActions(...)`生成(`TerminalHostScreen.kt`の`TerminalPaneScreen`)をそのまま通る。
 *
 * ライフサイクル台本([WiringContractTest])とは別クラスにして、UI側のflakeが配線の結果を隠さないようにする。
 * 待ち合わせはすべて`composeTestRule.waitUntil`(sleep無し)。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class WiringContractComposeTest {
    @get:Rule val composeTestRule = createComposeRule()

    private lateinit var vm: TerminalTabsViewModel
    private lateinit var testScheduler: TestCoroutineScheduler
    private val panes = mutableListOf<RecordingOrchestrator>()

    @Before
    fun setup() {
        testScheduler = TestCoroutineScheduler()
        Dispatchers.setMain(UnconfinedTestDispatcher(testScheduler))
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking { Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) } }
        val sessionFactory: (AppExecutor, RebindFdSource, ConnectionProfile) -> TerminalSession = { _, _, _ ->
            val recording = RecordingOrchestrator()
            panes.add(recording)
            testTerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> recording.fake.callback = cb; recording.proxy })
        }
        vm = TerminalTabsViewModel(app, DumbAppExecutor(), sessionFactory, UnconfinedTestDispatcher(testScheduler))
    }

    @After
    fun teardown() {
        Dispatchers.resetMain()
    }

    private fun minimalScreenUpdate() = ScreenUpdate(
        0u, 80u, 24u, emptyList(), 0u, 0u, null, null, null, null, false, false, false,
        MouseReportingMode.OFF, false, false, false, true, 0uL, 0uL, NotifyKind.INFO, "", "",
        0uL, PanelKind.NONE, "", "", emptyList(),
        CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u, null,
    )

    private fun waitFor(what: String, condition: () -> Boolean) = waitFor({ what }, condition)

    private fun waitFor(what: () -> String, condition: () -> Boolean) {
        try {
            composeTestRule.waitUntil(10_000) { condition() }
        } catch (e: Throwable) {
            throw AssertionError("待機がタイムアウト: ${what()}", e)
        }
    }

    private fun RecordingOrchestrator.awaitRecorded(method: String) =
        waitFor({ "$method がRustへ届くこと (記録=${recorded()})" }) { method in recorded() }

    private fun clickText(text: String) {
        waitFor("「$text」が表示されること") { composeTestRule.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty() }
        composeTestRule.onAllNodesWithText(text).onFirst().performSemanticsAction(SemanticsActions.OnClick)
    }

    /** 上部バー(ステータス行)は無操作2.5秒で自動的に隠れるので、本番と同じ単一指ドラッグで再表示する。 */
    private fun revealChrome() {
        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { swipeDown() }
    }

    @Test
    fun uiActions_reachRustFromTheRealTerminalHostScreen() {
        vm.openTab(ConnectionProfile(label = "alpha", host = "alpha.example.com", username = "user", authType = "password"), "pass")
        composeTestRule.setContent { TerminalHostScreen(onAllTabsClosed = {}, onNavigateToProfileList = {}, tabsVm = vm) }
        waitFor("セッションの生成と接続開始") { panes.isNotEmpty() && "connect" in panes[0].recorded() }
        val rec = panes[0]
        rec.fake.simulateConnected()
        rec.fake.simulateScreenUpdate(minimalScreenUpdate())
        waitFor("ターミナル描画") { composeTestRule.onAllNodesWithTag("terminalCanvas").fetchSemanticsNodes().isNotEmpty() }

        // フォーカスレポーティング(#60): LaunchedEffect(isActive, hasFocus)から生の可視/フォーカス状態が届く。
        rec.awaitRecorded("notifyFocusChange")

        // OSC 133: ライブ画面のタップでプロンプト上のカーソル移動(マウスレポーティングOFF)。
        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { click(center) }
        rec.awaitRecorded("clickToPromptCursor")

        // Ctrlキー行のボタン(横スクロール内なので座標クリックではなく意味木のOnClickで押す)。
        clickText("前のプロンプト")
        rec.awaitRecorded("jumpToPreviousPrompt")
        clickText("次のプロンプト")
        rec.awaitRecorded("jumpToNextPrompt")
        clickText("出力コピー")
        rec.awaitRecorded("copyLastCommandOutput")

        // trzsz(ダウンロード): 受信開始 → キャンセル → 完了通知の「閉じる」。
        rec.fake.simulateTrzszRequest("t1", "download", "wiring.txt", 10uL)
        clickText("受信開始")
        rec.awaitRecorded("trzszAcceptDownload")
        clickText("キャンセル")
        rec.awaitRecorded("trzszCancel")
        rec.fake.simulateTrzszFinished("t1", success = true)
        clickText("閉じる")
        rec.awaitRecorded("trzszDismiss")

        // スクロールバック検索(#66): 検索バーを開いてクエリを入力する。
        clickText("検索")
        waitFor("検索バーの入力欄") { composeTestRule.onAllNodes(hasSetTextAction()).fetchSemanticsNodes().isNotEmpty() }
        composeTestRule.onAllNodes(hasSetTextAction()).onFirst().performTextInput("needle")
        rec.awaitRecorded("searchScrollback")

        // 上部バー: セルラーへフェイルオーバー中だけ出る「今すぐWiFiに戻す」と、ファイルブラウザ(#17)。
        rec.fake.callback!!.onRebindStateChanged(RebindPublicState.FAILED_OVER_TO_CELLULAR)
        revealChrome()
        clickText("今すぐWiFiに戻す")
        rec.awaitRecorded("forceReturnToWifi")
        revealChrome()
        clickText("ファイル")
        rec.awaitRecorded("filePreviewRequest")

        // 自動再接続中だけ出る「中止」(未接続になると上部バーは強制的に再表示される)。
        rec.fake.simulateReconnecting()
        clickText("中止")
        rec.awaitRecorded("cancelReconnect")

        val expected = WiringContract.sessionOrchestrator
            .filterValues { it is Wiring.UiAction && it.composeExemptRef == null }.keys
        val missing = expected - rec.recorded()
        assertTrue(
            "UiActionに分類したのにこの台本で発火させていない/届かない入口: $missing " +
                "(新しいUiActionを分類したら台本に1手順足すこと。駆動できないものだけcomposeExemptRef付きで除外)",
            missing.isEmpty(),
        )
    }
}
