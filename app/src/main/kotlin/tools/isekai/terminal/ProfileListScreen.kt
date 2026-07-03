package tools.isekai.terminal

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.ui.DeleteConfirmDialog
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.setTerminalTheme

@Composable
fun ProfileListScreen(
    onConnect: (profile: ConnectionProfile, password: String?) -> Unit,
    onAddProfile: () -> Unit,
    onEditProfile: (ConnectionProfile) -> Unit,
    onManageKeys: () -> Unit = {},
    onManageSnippets: () -> Unit = {},
    // Rust 側への実際の反映は差し替え可能にしておく（テストでは native 呼び出しを避けるため no-op を注入する）
    applyTerminalTheme: (TerminalTheme) -> Unit = { theme ->
        setTerminalTheme(theme.ansi16Argb(), theme.foregroundArgb(), theme.backgroundArgb())
    },
) {
    val vm: ProfileListViewModel = viewModel()
    val profiles by vm.profiles.collectAsStateWithLifecycle()
    val passwordTarget by vm.passwordTarget.collectAsStateWithLifecycle()
    val deleteTarget by vm.deleteTarget.collectAsStateWithLifecycle()

    // プロファイル編集画面から戻ってきたときに一覧を最新化する
    // (ProfileListViewModel は NavHost 上で使い回されるため init だけでは再取得されない)
    LifecycleResumeEffect(Unit) {
        vm.loadProfiles()
        onPauseOrDispose {}
    }

    // 配色テーマはプロファイル毎ではなくグローバル設定として永続化する
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("tssh_ui", android.content.Context.MODE_PRIVATE) }
    var currentThemeName by remember {
        mutableStateOf(prefs.getString(TerminalThemes.PREF_KEY, null) ?: TerminalThemes.DEFAULT_DARK.name)
    }
    var showThemeDialog by remember { mutableStateOf(false) }

    Scaffold(
        topBar = {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .safeDrawingPadding()
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                horizontalArrangement = Arrangement.End,
            ) {
                TextButton(onClick = { showThemeDialog = true }) { Text("配色") }
                TextButton(onClick = onManageSnippets) { Text("定型") }
                TextButton(onClick = onManageKeys) { Text("鍵管理") }
            }
        },
        floatingActionButton = {
            FloatingActionButton(onClick = onAddProfile) {
                Text("＋", fontSize = 24.sp)
            }
        }
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding)
        ) {
            if (profiles.isEmpty()) {
                Text(
                    text = "「＋」をタップして接続先を追加",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.align(Alignment.Center),
                )
            } else {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(8.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    items(profiles, key = { it.id }) { profile ->
                        ProfileCard(
                            profile = profile,
                            onTap = {
                                if (profile.authType == "password") {
                                    RemoteLogger.i("TsshProfile", "tap → password dialog: '${profile.label}' ${profile.username}@${profile.host}:${profile.port}")
                                    vm.requestPasswordConnect(profile)
                                } else {
                                    RemoteLogger.i("TsshProfile", "tap → key connect: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} keyId=${profile.keyId}")
                                    onConnect(profile, null)
                                }
                            },
                            onEdit = { RemoteLogger.i("TsshProfile", "edit: '${profile.label}' id=${profile.id}"); onEditProfile(profile) },
                            onDelete = { vm.requestDelete(profile) },
                        )
                    }
                }
            }
        }
    }

    passwordTarget?.let { target ->
        PasswordDialog(
            label = target.label,
            onDismiss = { vm.dismissPassword() },
            onConfirm = { password ->
                vm.dismissPassword()
                onConnect(target, password)
            },
        )
    }

    deleteTarget?.let { target ->
        DeleteConfirmDialog(
            title = "削除確認",
            message = "「${target.label}」を削除しますか？",
            onConfirm = { vm.confirmDelete(target) },
            onDismiss = { vm.dismissDelete() },
        )
    }

    if (showThemeDialog) {
        TerminalThemeDialog(
            current = currentThemeName,
            onSelect = { theme ->
                currentThemeName = theme.name
                prefs.edit().putString(TerminalThemes.PREF_KEY, theme.name).apply()
                applyTerminalTheme(theme)
            },
            onDismiss = { showThemeDialog = false },
        )
    }
}

@Composable
private fun TerminalThemeDialog(
    current: String,
    onSelect: (TerminalTheme) -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("配色テーマ") },
        text = {
            Column {
                TerminalThemes.ALL.forEach { theme ->
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .clickable { onSelect(theme); onDismiss() }
                            .padding(vertical = 6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        RadioButton(
                            selected = theme.name == current,
                            onClick = { onSelect(theme); onDismiss() },
                        )
                        Spacer(Modifier.width(4.dp))
                        Text(theme.name)
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = onDismiss) { Text("閉じる") }
        },
    )
}

@Composable
private fun ProfileCard(
    profile: ConnectionProfile,
    onTap: () -> Unit,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onTap),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = profile.label,
                    fontWeight = FontWeight.Bold,
                    fontSize = 16.sp,
                )
                Spacer(Modifier.width(2.dp))
                Text(
                    text = "${profile.username}@${profile.host}:${profile.port}",
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Text(
                    text = if (profile.authType == "key") "鍵認証" else "パスワード",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            TextButton(onClick = onEdit) { Text("編集") }
            TextButton(onClick = onDelete) { Text("削除") }
        }
    }
}

@Composable
private fun PasswordDialog(
    label: String,
    onDismiss: () -> Unit,
    onConfirm: (String) -> Unit,
) {
    var password by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("パスワード入力") },
        text = {
            Column {
                Text("「$label」のパスワード")
                Spacer(Modifier.width(8.dp))
                OutlinedTextField(
                    value = password,
                    onValueChange = { password = it },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(onClick = {
                RemoteLogger.i("TsshProfile", "password dialog confirmed for: '$label'")
                onConfirm(password)
            }) { Text("接続") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("キャンセル") }
        },
    )
}
