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
import tools.isekai.terminal.util.RemoteLogger

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        RemoteLogger.i("MainActivity", "app started")
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
}

@Composable
fun AppRoot() {
    val navController = rememberNavController()
    val navVm: AppNavViewModel = viewModel()

    NavHost(navController = navController, startDestination = AppRoutes.PROFILE_LIST) {

        composable(AppRoutes.PROFILE_LIST) {
            RemoteLogger.i("TsshNav", "→ ProfileList")
            ProfileListScreen(
                onConnect = { profile, password ->
                    RemoteLogger.i("TsshNav", "ProfileList → Terminal profile='${profile.label}' authType=${profile.authType}")
                    navVm.pendingProfile = profile
                    navVm.pendingPassword = password
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
            )
        }

        composable(AppRoutes.TERMINAL) {
            RemoteLogger.i("TsshNav", "→ Terminal(profile='${navVm.pendingProfile?.label}' host=${navVm.pendingProfile?.host})")
            TerminalScreen(
                profile = navVm.pendingProfile,
                password = navVm.pendingPassword,
                onBack = { navController.popBackStack() },
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
    }
}
