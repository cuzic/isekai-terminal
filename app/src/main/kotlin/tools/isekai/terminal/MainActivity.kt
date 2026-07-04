package tools.isekai.terminal

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.setTerminalTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        RemoteLogger.i("MainActivity", "app started")
        restorePersistedTerminalTheme()
        enableEdgeToEdge()
        setContent {
            MaterialTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
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
        val prefs = getSharedPreferences("tssh_ui", MODE_PRIVATE)
        val theme = TerminalThemes.byName(prefs.getString(TerminalThemes.PREF_KEY, null))
        setTerminalTheme(theme.ansi16Argb(), theme.foregroundArgb(), theme.backgroundArgb())
    }
}

@Composable
fun AppRoot() {
    val navController = rememberNavController()
    val navVm: AppNavViewModel = viewModel()
    // Activity スコープ: NavHost の遷移をまたいでも同一インスタンスが使われるため、
    // ProfileList に一旦戻ってもバックグラウンドのタブ (共有 ForegroundService 上のセッション)
    // は生き続ける。
    val tabsVm: TerminalTabsViewModel = viewModel()

    NavHost(navController = navController, startDestination = AppRoutes.PROFILE_LIST) {

        composable(AppRoutes.PROFILE_LIST) {
            RemoteLogger.i("TsshNav", "→ ProfileList")
            ProfileListScreen(
                onConnect = { profile, password, jumpPassword ->
                    RemoteLogger.i("TsshNav", "ProfileList → Terminal profile='${profile.label}' authType=${profile.authType}")
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
                // Phase 12 P2-1: アプリ全体の既定テーマ変更を、まだ個別上書きしていない
                // (isThemeOverridden=false の)タブにも反映する。tabsVm は Activity スコープ
                // なので、まだ1つもタブが無い状態(list が空)でも安全に呼べる。
                applyTerminalTheme = { theme ->
                    setTerminalTheme(theme.ansi16Argb(), theme.foregroundArgb(), theme.backgroundArgb())
                    tabsVm.applyGlobalThemeToNonOverriddenTabs(theme)
                },
            )
        }

        composable(AppRoutes.TERMINAL) {
            RemoteLogger.i("TsshNav", "→ Terminal (tabs=${tabsVm.tabs.value.size})")
            TerminalHostScreen(
                tabsVm = tabsVm,
                onAllTabsClosed = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.PROFILE_EDIT) {
            val editing = navVm.pendingEditProfile
            RemoteLogger.i("TsshNav", "→ ${if (editing == null) "ProfileEdit(new)" else "ProfileEdit(id=${editing.id} '${editing.label}')"}")
            ProfileEditScreen(
                profile = editing,
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.KEY_LIST) {
            RemoteLogger.i("TsshNav", "→ KeyList")
            KeyListScreen(
                onImportKey = { navController.navigate(AppRoutes.KEY_IMPORT) },
                onBack = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.KEY_IMPORT) {
            RemoteLogger.i("TsshNav", "→ KeyImport")
            KeyImportScreen(
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }

        composable(AppRoutes.SNIPPET_LIST) {
            RemoteLogger.i("TsshNav", "→ SnippetList")
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
            RemoteLogger.i("TsshNav", "→ ${if (editing == null) "SnippetEdit(new)" else "SnippetEdit(id=${editing.id} '${editing.label}')"}")
            SnippetEditScreen(
                snippet = editing,
                onSave = { navController.popBackStack() },
                onCancel = { navController.popBackStack() },
            )
        }
    }
}
