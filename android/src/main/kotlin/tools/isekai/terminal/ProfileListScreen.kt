package tools.isekai.terminal

import android.app.Activity
import android.net.Uri
import android.os.Build
import android.provider.OpenableColumns
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Menu
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import tools.isekai.terminal.data.AuthType
import tools.isekai.terminal.data.BatteryGuidanceSettings
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.HostKeySettings
import tools.isekai.terminal.data.needsPasswordPrompt
import tools.isekai.terminal.input.KeyboardLayoutMode
import tools.isekai.terminal.ui.BackgroundReliabilityDialog
import tools.isekai.terminal.ui.DeleteConfirmDialog
import tools.isekai.terminal.ui.ListItemCard
import tools.isekai.terminal.ui.RadioChoiceDialog
import tools.isekai.terminal.ui.TerminalFontSettings
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.applyTo
import tools.isekai.terminal.util.BatteryOptimization
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.setCtlSocketForwardEnabled
import uniffi.isekai_terminal_core.setTerminalTheme

@Composable
fun ProfileListScreen(
    onConnect: (profile: ConnectionProfile, password: String?, jumpPassword: String?) -> Unit,
    onAddProfile: () -> Unit,
    onEditProfile: (ConnectionProfile) -> Unit,
    onManageKeys: () -> Unit = {},
    onManageSnippets: () -> Unit = {},
    onManageKeySequences: () -> Unit = {},
    // Rust 側への実際の反映は差し替え可能にしておく（テストでは native 呼び出しを避けるため no-op を注入する）
    applyTerminalTheme: (TerminalTheme) -> Unit = { theme -> theme.applyTo(::setTerminalTheme) },
    // tmux迂回control-planeのRust側プロセスグローバル状態への反映も同様に差し替え可能にする。
    applyCtlSocketForwardEnabled: (Boolean) -> Unit = ::setCtlSocketForwardEnabled,
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
    val prefs = remember { context.getSharedPreferences("isekai_terminal_ui", android.content.Context.MODE_PRIVATE) }
    var currentThemeName by remember {
        mutableStateOf(prefs.getString(TerminalThemes.PREF_KEY, null) ?: TerminalThemes.DEFAULT_DARK.name)
    }
    var showThemeDialog by remember { mutableStateOf(false) }
    var showMenu by remember { mutableStateOf(false) }
    var showSecurityDialog by remember { mutableStateOf(false) }
    var showKeyboardLayoutDialog by remember { mutableStateOf(false) }
    // 項目2: OEMバッテリー最適化への案内UIの恒常入口(nagなし、いつでも自分で開ける)。
    // コールドスタート時の自動案内([MainActivity.AppRoot]・`TerminalTabsViewModel`)とは
    // 別経路で、こちらはローカルなComposeの開閉状態のみを持つ。
    var showBackgroundReliabilityDialog by remember { mutableStateOf(false) }

    // 外部/BluetoothキーボードのJIS/US配列モード。テーマと同じく、どのホストに
    // 接続していても使う物理キーボード側の特性なのでグローバル設定として永続化する。
    var keyboardLayoutMode by remember {
        mutableStateOf(KeyboardLayoutMode.fromPrefValue(prefs.getString(KeyboardLayoutMode.PREF_KEY, null)))
    }

    // カスタム端末フォント(TTF/OTF)もテーマと同じくグローバル設定として永続化するが、
    // 「配色テーマ(TerminalTheme.kt)とは分離する」というスコープ済み方針により
    // 独立した TerminalFontSettings(SharedPreferences のキーのみ分離)として扱う。
    var showFontDialog by remember { mutableStateOf(false) }
    var currentFontFileName by remember { mutableStateOf(prefs.getString(TerminalFontSettings.PREF_KEY, null)) }
    var fontErrorMessage by remember { mutableStateOf<String?>(null) }
    val coroutineScope = rememberCoroutineScope()
    val fontPickerLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocument(),
    ) { uri: Uri? ->
        if (uri != null) {
            val displayName = queryDisplayName(context, uri)
            RemoteLogger.i("IsekaiTerminalFont", "font file selected via SAF: $displayName uri=$uri")
            coroutineScope.launch {
                val result = withContext(Dispatchers.IO) {
                    TerminalFontSettings.importFont(context, prefs, uri, displayName)
                }
                when (result) {
                    is TerminalFontSettings.ImportResult.Success -> {
                        RemoteLogger.i("IsekaiTerminalFont", "font import succeeded: ${result.fileName}")
                        currentFontFileName = result.fileName
                        fontErrorMessage = null
                    }
                    is TerminalFontSettings.ImportResult.Failure -> {
                        RemoteLogger.e("IsekaiTerminalFont", "font import failed: ${result.message}")
                        fontErrorMessage = result.message
                    }
                }
            }
        } else {
            RemoteLogger.i("IsekaiTerminalFont", "SAF font picker cancelled")
        }
    }

    // 画面の保護(FLAG_SECURE、#62)もプロファイル毎ではなくグローバル設定として永続化する。
    // 既定OFF(常時ONは一部ユーザに不便なため)のオプトイン機能。
    var screenProtectionEnabled by remember {
        mutableStateOf(prefs.getBoolean(PREF_KEY_SCREEN_PROTECTION, false))
    }

    // リモートからの OSC 52 クリップボード書き込み(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)も
    // 画面の保護と同じくグローバル設定として永続化する。既定OFF(クリップボード
    // ハイジャックのリスクがあるため)のオプトイン機能。
    var remoteClipboardWriteEnabled by remember {
        mutableStateOf(prefs.getBoolean(PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE, false))
    }

    // リモートからの OSC 52 query(クリップボード読み出し)応答も push とは別にopt-inする
    // (デバイス側の機密情報がリモートへ流出するリスクがあるため、既定OFF)。
    var remoteClipboardPullEnabled by remember {
        mutableStateOf(prefs.getBoolean(PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL, false))
    }

    // tmux 迂回 control-plane(russh の streamlocal forward、`ISEKAI_PIPE_DESIGN.md`
    // §8 Epic M)。既定OFF(常時ONにする理由が無いため)。トグル時にRust側の
    // プロセスグローバル状態へ即座に反映する([MainActivity]の起動時復元と対になる)。
    var ctlSocketForwardEnabled by remember {
        mutableStateOf(prefs.getBoolean(PREF_KEY_ENABLE_CTL_SOCKET_FORWARD, false))
    }

    // メニュー内のON/OFFトグル項目(画面の保護・クリップボード書込/送信・tmux迂回control-plane)は
    // 「現在値からラベルを組み立て、タップで反転してprefsへ即座に永続化する」という骨格が
    // 4箇所とも同一だったため、この局所ヘルパーへ切り出した。反転後の値をprefsへの永続化以外にも
    // 反映したい場合(画面保護のFLAG_SECURE適用、tmux迂回control-planeのRust側グローバル状態反映)は
    // [onToggled] で行う。
    @Composable
    fun toggleMenuItem(label: String, value: Boolean, key: String, onToggled: (Boolean) -> Unit) {
        DropdownMenuItem(
            text = { Text(if (value) "$label: ON" else "$label: OFF") },
            onClick = {
                showMenu = false
                val newValue = !value
                prefs.edit().putBoolean(key, newValue).apply()
                onToggled(newValue)
            },
        )
    }

    Scaffold(
        topBar = {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .safeDrawingPadding()
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                horizontalArrangement = Arrangement.End,
            ) {
                Box {
                    IconButton(onClick = { showMenu = true }) {
                        Icon(Icons.Default.Menu, contentDescription = "メニュー")
                    }
                    DropdownMenu(expanded = showMenu, onDismissRequest = { showMenu = false }) {
                        DropdownMenuItem(
                            text = { Text("配色") },
                            onClick = { showMenu = false; showThemeDialog = true },
                        )
                        DropdownMenuItem(
                            text = { Text("フォント") },
                            onClick = { showMenu = false; fontErrorMessage = null; showFontDialog = true },
                        )
                        DropdownMenuItem(
                            text = { Text("キーボード配列: ${keyboardLayoutMode.label()}") },
                            onClick = { showMenu = false; showKeyboardLayoutDialog = true },
                        )
                        DropdownMenuItem(
                            text = { Text("定型") },
                            onClick = { showMenu = false; onManageSnippets() },
                        )
                        DropdownMenuItem(
                            text = { Text("打鍵列") },
                            onClick = { showMenu = false; onManageKeySequences() },
                        )
                        DropdownMenuItem(
                            text = { Text("鍵管理") },
                            onClick = { showMenu = false; onManageKeys() },
                        )
                        toggleMenuItem("画面の保護", screenProtectionEnabled, PREF_KEY_SCREEN_PROTECTION) { newValue ->
                            screenProtectionEnabled = newValue
                            (context as? Activity)?.let { applyScreenProtection(it, newValue) }
                        }
                        toggleMenuItem(
                            "リモートからのクリップボード書込",
                            remoteClipboardWriteEnabled,
                            PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE,
                        ) { newValue -> remoteClipboardWriteEnabled = newValue }
                        toggleMenuItem(
                            "リモートへのクリップボード送信",
                            remoteClipboardPullEnabled,
                            PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL,
                        ) { newValue -> remoteClipboardPullEnabled = newValue }
                        toggleMenuItem("tmux迂回control-plane", ctlSocketForwardEnabled, PREF_KEY_ENABLE_CTL_SOCKET_FORWARD) { newValue ->
                            ctlSocketForwardEnabled = newValue
                            applyCtlSocketForwardEnabled(newValue)
                        }
                        DropdownMenuItem(
                            text = { Text("セキュリティ") },
                            onClick = { showMenu = false; showSecurityDialog = true },
                        )
                        DropdownMenuItem(
                            text = { Text("バックグラウンド動作") },
                            onClick = { showMenu = false; showBackgroundReliabilityDialog = true },
                        )
                    }
                }
            }
        },
        floatingActionButton = {
            FloatingActionButton(onClick = onAddProfile, modifier = Modifier.testTag("addProfileFab")) {
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
                                if (profile.needsPasswordPrompt) {
                                    RemoteLogger.i("IsekaiTerminalProfile", "tap → password dialog: '${profile.label}' ${profile.username}@${profile.host}:${profile.port}")
                                    vm.requestPasswordConnect(profile)
                                } else {
                                    RemoteLogger.i("IsekaiTerminalProfile", "tap → key connect: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} keyId=${profile.keyId}")
                                    onConnect(profile, null, null)
                                }
                            },
                            onEdit = { RemoteLogger.i("IsekaiTerminalProfile", "edit: '${profile.label}' id=${profile.id}"); onEditProfile(profile) },
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
            showMainField = target.authTypeEnum == AuthType.PASSWORD,
            jumpLabel = if (target.usesJumpHost && target.jumpAuthTypeEnum == AuthType.PASSWORD) target.jumpHost else null,
            onDismiss = { vm.dismissPassword() },
            onConfirm = { password, jumpPassword ->
                vm.dismissPassword()
                onConnect(target, password, jumpPassword)
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

    if (showSecurityDialog) {
        SecuritySettingsDialog(
            context = context,
            onDismiss = { showSecurityDialog = false },
        )
    }

    if (showFontDialog) {
        FontSettingsDialog(
            currentFontLabel = currentFontFileName ?: "デフォルト (Monospace)",
            errorMessage = fontErrorMessage,
            onPickFont = { fontPickerLauncher.launch(arrayOf("*/*")) },
            onResetFont = {
                TerminalFontSettings.clearStoredFont(context, prefs)
                currentFontFileName = null
                fontErrorMessage = null
                RemoteLogger.i("IsekaiTerminalFont", "font reset to default (Typeface.MONOSPACE)")
            },
            onDismiss = { showFontDialog = false },
        )
    }

    if (showKeyboardLayoutDialog) {
        KeyboardLayoutDialog(
            current = keyboardLayoutMode,
            onSelect = { mode ->
                keyboardLayoutMode = mode
                prefs.edit().putString(KeyboardLayoutMode.PREF_KEY, mode.name).apply()
            },
            onDismiss = { showKeyboardLayoutDialog = false },
        )
    }

    if (showBackgroundReliabilityDialog) {
        var optedOut by remember { mutableStateOf(BatteryGuidanceSettings.isOptedOut(context)) }
        BackgroundReliabilityDialog(
            manufacturer = Build.MANUFACTURER ?: "",
            optedOut = optedOut,
            onOptOutChanged = { newValue ->
                optedOut = newValue
                BatteryGuidanceSettings.setOptedOut(context, newValue)
            },
            onOpenSettings = { BatteryOptimization.openIgnoreBatteryOptimizationSettings(context) },
            onDismiss = { showBackgroundReliabilityDialog = false },
        )
    }
}

/** SAF の [Uri] の表示名(元ファイル名)を [OpenableColumns.DISPLAY_NAME] から取得する。
 *  取得できない場合は [Uri.getLastPathSegment] にフォールバックする。 */
private fun queryDisplayName(context: android.content.Context, uri: Uri): String? {
    return try {
        context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
            if (cursor.moveToFirst()) {
                val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (idx >= 0) cursor.getString(idx) else null
            } else null
        } ?: uri.lastPathSegment
    } catch (e: Exception) {
        uri.lastPathSegment
    }
}

/**
 * ホスト鍵関連のセキュリティ設定。初回接続(Unknown host key)を確認ダイアログ無しで
 * 自動信頼するかどうかのオプトアウト設定([HostKeySettings])のみを扱う
 * (ホスト鍵変更検知の警告は既に堅牢なため常時有効・設定不可)。
 */
@Composable
private fun SecuritySettingsDialog(
    context: android.content.Context,
    onDismiss: () -> Unit,
) {
    var autoTrust by remember { mutableStateOf(HostKeySettings.isAutoTrustNewHostKeysEnabled(context)) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("セキュリティ") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(modifier = Modifier.weight(1f)) {
                        Text("初回接続を自動的に信頼する", fontSize = 14.sp)
                        Text(
                            "オフ(既定)の場合、初めて接続するホストの fingerprint を毎回確認します。",
                            fontSize = 11.sp,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    Switch(
                        checked = autoTrust,
                        onCheckedChange = { enabled ->
                            autoTrust = enabled
                            HostKeySettings.setAutoTrustNewHostKeysEnabled(context, enabled)
                        },
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = onDismiss) { Text("閉じる") }
        },
    )
}

/**
 * カスタム端末フォント(TTF/OTF)の選択・削除ダイアログ。配色テーマ([TerminalThemeDialog])とは
 * 独立した設定として、選択→内部ストレージへコピー→[Typeface][android.graphics.Typeface]としての
 * 検証、というフローを [TerminalFontSettings.importFont] に委譲する(実際のI/Oはバックグラウンド
 * スレッドで行われ、結果に応じて [errorMessage] が表示される)。
 */
@Composable
internal fun FontSettingsDialog(
    currentFontLabel: String,
    errorMessage: String?,
    onPickFont: () -> Unit,
    onResetFont: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("フォント") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("現在のフォント: $currentFontLabel", fontSize = 13.sp)
                Text(
                    "端末上の TTF/OTF ファイル(Nerd Font 等)を選択してターミナルのフォントに" +
                        "設定できます。読み込めないファイルを選んだ場合は既定のフォントのまま" +
                        "変更されません。",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                errorMessage?.let {
                    Text(it, color = MaterialTheme.colorScheme.error, fontSize = 12.sp)
                }
                Button(
                    onClick = onPickFont,
                    modifier = Modifier.fillMaxWidth().testTag("fontPickButton"),
                ) { Text("TTF/OTF ファイルを選択") }
                OutlinedButton(
                    onClick = onResetFont,
                    modifier = Modifier.fillMaxWidth().testTag("fontResetButton"),
                ) { Text("デフォルトに戻す") }
            }
        },
        confirmButton = {
            TextButton(onClick = onDismiss) { Text("閉じる") }
        },
    )
}

@Composable
internal fun TerminalThemeDialog(
    current: String,
    onSelect: (TerminalTheme) -> Unit,
    onDismiss: () -> Unit,
) {
    RadioChoiceDialog(
        title = "配色テーマ",
        items = TerminalThemes.ALL,
        label = { it.name },
        isSelected = { it.name == current },
        onSelect = onSelect,
        onDismiss = onDismiss,
    )
}

/**
 * 外部/BluetoothキーボードのJIS/US配列モード選択ダイアログ。既定は「自動判定」
 * ([KeyboardLayoutDetector]によるハードウェア構成からの推定)。推定が外れる端末向けに
 * JIS/USへの手動固定を用意する。
 */
@Composable
internal fun KeyboardLayoutDialog(
    current: KeyboardLayoutMode,
    onSelect: (KeyboardLayoutMode) -> Unit,
    onDismiss: () -> Unit,
) {
    RadioChoiceDialog(
        title = "キーボード配列",
        items = KeyboardLayoutMode.entries,
        label = { it.label() },
        isSelected = { it == current },
        onSelect = onSelect,
        onDismiss = onDismiss,
        description = "接続中の外部キーボードが¥キー/ろキーを備えているかで自動判定します。" +
            "うまく検出されない場合はここで固定してください。",
    )
}

@Composable
private fun ProfileCard(
    profile: ConnectionProfile,
    onTap: () -> Unit,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
) {
    ListItemCard(
        title = profile.label,
        subtitle = "${profile.username}@${profile.host}:${profile.port}",
        caption = if (profile.authTypeEnum == AuthType.KEY) "鍵認証" else "パスワード",
        onTap = onTap,
        onEdit = onEdit,
        onDelete = onDelete,
        editTestTag = "profileEditButton",
        deleteTestTag = "profileDeleteButton",
    )
}

/**
 * [showMainField] が false のときは対象ホスト自体は鍵認証だが、踏み台
 * ([jumpLabel] が non-null)がパスワード認証のため、踏み台分のフィールドだけを表示する。
 */
@Composable
internal fun PasswordDialog(
    label: String,
    showMainField: Boolean,
    jumpLabel: String?,
    onDismiss: () -> Unit,
    onConfirm: (password: String, jumpPassword: String?) -> Unit,
) {
    var password by remember { mutableStateOf("") }
    var jumpPassword by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("パスワード入力") },
        text = {
            Column {
                if (showMainField) {
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
                if (jumpLabel != null) {
                    if (showMainField) Spacer(Modifier.width(8.dp))
                    Text("踏み台「$jumpLabel」のパスワード")
                    Spacer(Modifier.width(8.dp))
                    OutlinedTextField(
                        value = jumpPassword,
                        onValueChange = { jumpPassword = it },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                RemoteLogger.i("IsekaiTerminalProfile", "password dialog confirmed for: '$label'")
                onConfirm(password, jumpLabel?.let { jumpPassword })
            }) { Text("接続") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("キャンセル") }
        },
    )
}
