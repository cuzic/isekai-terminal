package tools.isekai.terminal

import android.app.Activity
import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.applyTo
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.setCtlSocketForwardEnabled
import uniffi.isekai_terminal_core.setTerminalTheme

/** `SharedPreferences("isekai_terminal_ui")` に保存する「画面の保護」(FLAG_SECURE) 設定のキー。 */
const val PREF_KEY_SCREEN_PROTECTION = "screen_protection_enabled"

/**
 * `SharedPreferences("isekai_terminal_ui")` に保存する「リモートからのクリップボード書き込み
 * (OSC 52)を許可する」設定のキー。既定OFFのオプトイン(`ISEKAI_PIPE_DESIGN.md` §8 Epic M:
 * リモートが仕込んだコマンドを気づかず貼り付けて実行してしまう「クリップボードハイジャック」
 * のリスクがあるため)。[applyScreenProtection]と違い window へ即時反映する状態を持たないので、
 * アプリ起動時の復元処理は不要——[TerminalTabsViewModel]がセッション生成時に都度読む。
 */
const val PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE = "allow_remote_clipboard_write"

/**
 * `SharedPreferences("isekai_terminal_ui")` に保存する「リモートからのクリップボード読み出し
 * (OSC 52 query への応答)を許可する」設定のキー。既定OFFのオプトイン(デバイス側の
 * クリップボード内容(パスワード等を含みうる)がリモートへ流出するリスクがあるため、
 * 書き込み側([PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE])とは別々にopt-inできるようにしている、
 * `ISEKAI_PIPE_DESIGN.md` §8 Epic M参照)。
 */
const val PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL = "allow_remote_clipboard_pull"

/**
 * `SharedPreferences("isekai_terminal_ui")` に保存する「tmux 迂回 control-plane(russh の
 * streamlocal forward経由でリモートの`isekai-pipe ctl title|clip push`を直接受け取る、
 * `ISEKAI_PIPE_DESIGN.md` §8 Epic M)を有効にする」設定のキー。既定OFFのオプトイン。
 * [restorePersistedScreenProtection]と同様、Rust側はプロセスグローバルな状態
 * ([setCtlSocketForwardEnabled])として持つため、アプリ起動時に一度反映すればよい。
 */
const val PREF_KEY_ENABLE_CTL_SOCKET_FORWARD = "enable_ctl_socket_forward"

/**
 * 画面の保護(スクリーンショット・画面録画・「最近使ったアプリ」のサムネイルを禁止する
 * [WindowManager.LayoutParams.FLAG_SECURE])を適用/解除する。
 *
 * 既定OFFのオプトイン機能(常時ONは一部ユーザに不便なため。#62)。アプリ全体で1枚の window
 * しか持たないため、ここで一度適用すればプロファイル一覧・パスワード入力ダイアログ・
 * ターミナルセッションなど、以降遷移する全画面に効く(最低限求められる「パスワード入力
 * ダイアログ表示中」「アクティブなターミナルセッション中」の保護も自動的に満たす)。
 */
fun applyScreenProtection(activity: Activity, enabled: Boolean) {
    if (enabled) {
        activity.window.setFlags(WindowManager.LayoutParams.FLAG_SECURE, WindowManager.LayoutParams.FLAG_SECURE)
    } else {
        activity.window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }
}

/**
 * [MainActivity.restorePersistedCtlSocketForward]の中核ロジック。SharedPreferencesの
 * 読み取りとRust側([setCtlSocketForwardEnabled]、native/UniFFI呼び出し)への反映を分離し、
 * [apply]を差し替えればテストから native 呼び出し無しで検証できるようにする。
 */
internal fun restoreCtlSocketForwardEnabled(
    prefs: android.content.SharedPreferences,
    apply: (Boolean) -> Unit = ::setCtlSocketForwardEnabled,
) {
    apply(prefs.getBoolean(PREF_KEY_ENABLE_CTL_SOCKET_FORWARD, false))
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        RemoteLogger.i("MainActivity", "app started")
        restorePersistedTerminalTheme()
        restorePersistedScreenProtection()
        restorePersistedCtlSocketForward()
        enableEdgeToEdge()
        setContent {
            MaterialTheme {
                Surface(
                    // デバッグビルドのみ testTag を uiautomator dump 上の resource-id として
                    // 露出する(scripts/device_verify.sh がテキストではなく resource-id で
                    // 要素を掴めるようにするため)。リリースビルドでは無効(アクセシビリティ
                    // サービス経由で内部タグ名が外部に見える面を増やさない)。
                    modifier = Modifier
                        .fillMaxSize()
                        .semantics { testTagsAsResourceId = BuildConfig.DEBUG },
                    color = MaterialTheme.colorScheme.background
                ) {
                    AppRoot()
                }
            }
        }
    }

    /**
     * 前回選択した配色テーマ（グローバル設定、プロファイル毎ではない）を
     * Rust 側のテーマテーブル（`rust-core/src/theme.rs`、案C）へ復元する。
     * パレット自体は Rust 側のプロセス全体で共有されるグローバル状態のため、
     * アプリ起動直後に一度反映しておけば、以降に生成される全セッションに引き継がれる。
     */
    private fun restorePersistedTerminalTheme() {
        val prefs = getSharedPreferences("isekai_terminal_ui", MODE_PRIVATE)
        val theme = TerminalThemes.byName(prefs.getString(TerminalThemes.PREF_KEY, null))
        theme.applyTo(::setTerminalTheme)
    }

    /**
     * 前回設定した「画面の保護」(既定OFF) を、アプリ起動直後にこの Activity の window へ
     * 復元する。実行中のトグルは [applyScreenProtection] を直接呼ぶ側([ProfileListScreen] の
     * メニュー)が担当するので、ここは起動時の1回だけでよい。
     */
    private fun restorePersistedScreenProtection() {
        val prefs = getSharedPreferences("isekai_terminal_ui", MODE_PRIVATE)
        applyScreenProtection(this, prefs.getBoolean(PREF_KEY_SCREEN_PROTECTION, false))
    }

    /**
     * 前回設定した「tmux 迂回 control-plane」(既定OFF) を、Rust側のプロセスグローバルな
     * 状態(`set_ctl_socket_forward_enabled`)へ起動直後に一度反映する。実行中のトグルは
     * [ProfileListScreen] のメニューが直接 [setCtlSocketForwardEnabled] を呼ぶので、
     * ここは起動時の1回だけでよい。
     */
    private fun restorePersistedCtlSocketForward() {
        restoreCtlSocketForwardEnabled(getSharedPreferences("isekai_terminal_ui", MODE_PRIVATE))
    }
}

@Composable
fun AppRoot() {
    val navController = rememberNavController()
    val navVm: AppNavViewModel = viewModel()
    // Application(≒プロセス)スコープ: Activity が(バックグラウンド中のタスク破棄等で)
    // 再生成されても同一インスタンスが使われるため、ForegroundService 上のセッションを
    // 保持しているタブを見失わない。Activity スコープだと、Activity 再生成時に
    // TerminalTabsViewModel.onCleared() を経由せず古いインスタンスが破棄され、サーバー側の
    // SSH セッションだけが孤立して残るバグがあった(実機検証で発見、2026-07-12)。
    val application = LocalContext.current.applicationContext as IsekaiTerminalApplication
    val tabsVm: TerminalTabsViewModel = viewModel(
        viewModelStoreOwner = application,
        factory = ViewModelProvider.AndroidViewModelFactory.getInstance(application),
    )

    // 既にタブがある(=プロセスが生きたままActivityだけ再生成された)場合は、ProfileListを
    // 経由せず直接Terminalへ着地させる。
    val startDestination = if (tabsVm.tabs.value.isNotEmpty()) AppRoutes.TERMINAL else AppRoutes.PROFILE_LIST

    // タスク#14: プロセスkillからの黙示的セッション再アタッチは、`tabsVm`の`init{}`が
    // ioDispatcher上の非同期コルーチンでタブを復元する(`TerminalTabsViewModel.
    // restorePersistedReattachTabs`参照)。そのため上の`startDestination`の同期的な
    // 判定(この行より前の時点のtabs状態)だけでは、復元がこの初回コンポジションに
    // 間に合わずProfileListへ着地してしまう場合がある。復元が完了してtabsが空から
    // 非空になった時点でも、まだProfileList表示中であれば追ってTerminalへ遷移させる
    // (ユーザーが既に他画面へ手動遷移していた場合は上書きしない)。
    LaunchedEffect(navController) {
        tabsVm.tabs.collect { tabs ->
            if (tabs.isNotEmpty() && navController.currentDestination?.route == AppRoutes.PROFILE_LIST) {
                RemoteLogger.i("IsekaiTerminalNav", "implicit reattach produced a tab, navigating ProfileList → Terminal")
                navController.navigate(AppRoutes.TERMINAL)
            }
        }
    }

    NavHost(navController = navController, startDestination = startDestination) {

        composable(AppRoutes.PROFILE_LIST) {
            RemoteLogger.i("IsekaiTerminalNav", "→ ProfileList")
            ProfileListScreen(
                onConnect = { profile, password, jumpPassword ->
                    RemoteLogger.i("IsekaiTerminalNav", "ProfileList → Terminal profile='${profile.label}' authType=${profile.authType}")
                    tabsVm.openTab(profile, password, jumpPassword)
                    navController.navigate(AppRoutes.TERMINAL)
                },
                onAddProfile = {
                    navVm.pendingEditProfile = null
                    navController.navigate(AppRoutes.PROFILE_EDIT)
                },
                onEditProfile = { profile ->
                    navVm.pendingEditProfile = profile
                    navController.navigate(AppRoutes.PROFILE_EDIT)
                },
                onManageKeys = { navController.navigate(AppRoutes.KEY_LIST) },
                onManageSnippets = { navController.navigate(AppRoutes.SNIPPET_LIST) },
                onManageKeySequences = { navController.navigate(AppRoutes.KEY_SEQUENCE_LIST) },
                // Phase 12 P2-1: アプリ全体の既定テーマ変更を、まだ個別上書きしていない
                // (isThemeOverridden=false の)タブにも反映する。tabsVm は Application スコープ
                // なので、まだ1つもタブが無い状態(list が空)でも安全に呼べる。
                applyTerminalTheme = { theme ->
                    theme.applyTo(::setTerminalTheme)
                    tabsVm.applyGlobalThemeToNonOverriddenTabs(theme)
                },
            )
        }

        composable(AppRoutes.TERMINAL) {
            RemoteLogger.i("IsekaiTerminalNav", "→ Terminal (tabs=${tabsVm.tabs.value.size})")
            TerminalHostScreen(
                tabsVm = tabsVm,
                // popBackStack() だと、tabsVm復元によりTerminalがstartDestinationになっていた
                // 場合(戻り先が無い)に詰まるため、明示的にProfileListへ遷移してTerminalを
                // back stackから取り除く。
                onAllTabsClosed = {
                    navController.navigate(AppRoutes.PROFILE_LIST) {
                        popUpTo(AppRoutes.TERMINAL) { inclusive = true }
                        launchSingleTop = true
                    }
                },
            )
        }

        composable(AppRoutes.PROFILE_EDIT) {
            val editing = navVm.pendingEditProfile
            RemoteLogger.i("IsekaiTerminalNav", "→ ${if (editing == null) "ProfileEdit(new)" else "ProfileEdit(id=${editing.id} '${editing.label}')"}")
            ProfileEditScreen(
                profile = editing,
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.KEY_LIST) {
            RemoteLogger.i("IsekaiTerminalNav", "→ KeyList")
            KeyListScreen(
                onImportKey = { navController.navigate(AppRoutes.KEY_IMPORT) },
                onBack = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.KEY_IMPORT) {
            RemoteLogger.i("IsekaiTerminalNav", "→ KeyImport")
            KeyImportScreen(
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.SNIPPET_LIST) {
            RemoteLogger.i("IsekaiTerminalNav", "→ SnippetList")
            SnippetListScreen(
                onAddSnippet = {
                    navVm.pendingEditSnippet = null
                    navController.navigate(AppRoutes.SNIPPET_EDIT)
                },
                onEditSnippet = { snippet ->
                    navVm.pendingEditSnippet = snippet
                    navController.navigate(AppRoutes.SNIPPET_EDIT)
                },
                onBack = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.SNIPPET_EDIT) {
            val editing = navVm.pendingEditSnippet
            RemoteLogger.i("IsekaiTerminalNav", "→ ${if (editing == null) "SnippetEdit(new)" else "SnippetEdit(id=${editing.id} '${editing.label}')"}")
            SnippetEditScreen(
                snippet = editing,
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.KEY_SEQUENCE_LIST) {
            RemoteLogger.i("IsekaiTerminalNav", "→ KeySequenceList")
            KeySequenceListScreen(
                onAddKeySequence = {
                    navVm.pendingEditKeySequence = null
                    navController.navigate(AppRoutes.KEY_SEQUENCE_EDIT)
                },
                onEditKeySequence = { keySequence ->
                    navVm.pendingEditKeySequence = keySequence
                    navController.navigate(AppRoutes.KEY_SEQUENCE_EDIT)
                },
                onBack = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.KEY_SEQUENCE_EDIT) {
            val editing = navVm.pendingEditKeySequence
            RemoteLogger.i("IsekaiTerminalNav", "→ ${if (editing == null) "KeySequenceEdit(new)" else "KeySequenceEdit(id=${editing.id} '${editing.label}')"}")
            KeySequenceEditScreen(
                keySequence = editing,
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }
    }
}
