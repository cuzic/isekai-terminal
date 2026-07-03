package tools.isekai.terminal

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.ui.Alignment
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MenuAnchorType
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.ForwardType
import uniffi.tssh_core.PortForward
import uniffi.tssh_core.TransportPreference

/**
 * ポートフォワード編集欄用の入力中ドラフト([PortForward] は bindPort/remotePort が
 * UShort で空文字を表現できないため、テキスト入力中は String で持つ)。
 */
private data class ForwardDraft(
    val bindAddress: String = "127.0.0.1",
    val bindPort: String = "",
    val remoteHost: String = "",
    val remotePort: String = "",
)

private fun PortForward.toDraft() = ForwardDraft(
    bindAddress = bindAddress,
    bindPort = bindPort.toString(),
    remoteHost = remoteHost,
    remotePort = remotePort.toString(),
)

/** remoteHost 未入力や不正なポート番号の行は保存対象から除外する。 */
private fun ForwardDraft.toPortForwardOrNull(): PortForward? {
    val bp = bindPort.toIntOrNull() ?: return null
    val rp = remotePort.toIntOrNull() ?: return null
    if (remoteHost.isBlank() || bp !in 1..65535 || rp !in 1..65535) return null
    return PortForward(
        forwardType = ForwardType.LOCAL,
        bindAddress = bindAddress.ifBlank { "127.0.0.1" },
        bindPort = bp.toUShort(),
        remoteHost = remoteHost,
        remotePort = rp.toUShort(),
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileEditScreen(
    profile: ConnectionProfile? = null,
    onSave: () -> Unit,
    onCancel: () -> Unit,
) {
    val vm: ProfileEditViewModel = viewModel()
    val keys by vm.keys.collectAsStateWithLifecycle()
    val isSaving by vm.isSaving.collectAsStateWithLifecycle()

    var label by remember { mutableStateOf(profile?.label ?: "") }
    var host by remember { mutableStateOf(profile?.host ?: "") }
    var port by remember { mutableStateOf((profile?.port ?: 22).toString()) }
    var username by remember { mutableStateOf(profile?.username ?: "") }
    var authType by remember { mutableStateOf(profile?.authType ?: "password") }
    var keyId by remember { mutableStateOf(profile?.keyId) }
    var keyMenuExpanded by remember { mutableStateOf(false) }
    var transportPreference by remember { mutableStateOf(profile?.transportPreference ?: TransportPreference.PLAIN_SSH) }
    var tsshdPort by remember { mutableStateOf((profile?.tsshdPort ?: ConnectionProfile.DEFAULT_TSSHD_PORT).toString()) }
    var directAddress by remember { mutableStateOf(profile?.directAddress ?: "") }
    var enablePhysicalMultipath by remember { mutableStateOf(profile?.enablePhysicalMultipath ?: false) }
    var cellularRemoteAddress by remember { mutableStateOf(profile?.cellularRemoteAddress ?: "") }
    var enableUpstreamFailover by remember { mutableStateOf(profile?.enableUpstreamFailover ?: false) }
    var postConnectCommands by remember { mutableStateOf(profile?.postConnectCommands ?: "") }
    val forwardDrafts = remember {
        mutableStateListOf<ForwardDraft>().apply {
            profile?.forwards?.forEach { add(it.toDraft()) }
        }
    }

    val selectedKeyLabel = keys.firstOrNull { it.id == keyId }?.label ?: "鍵を選択"
    val canSave = label.isNotBlank() && host.isNotBlank() && username.isNotBlank() &&
        (authType == "password" || keyId != null)

    Column(
        modifier = Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            text = if (profile == null) "プロファイル追加" else "プロファイル編集",
            fontWeight = FontWeight.Bold,
            fontSize = 20.sp,
        )

        OutlinedTextField(
            value = label,
            onValueChange = { label = it },
            label = { Text("ラベル") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = host,
            onValueChange = { host = it },
            label = { Text("ホスト") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = port,
            onValueChange = { new -> port = new.filter { it.isDigit() }.take(5) },
            label = { Text("ポート") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = username,
            onValueChange = { username = it },
            label = { Text("ユーザー名") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )

        Text("認証方式")
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(
                selected = authType == "password",
                onClick = { authType = "password" },
                label = { Text("パスワード") },
            )
            FilterChip(
                selected = authType == "key",
                onClick = { authType = "key" },
                label = { Text("鍵認証") },
            )
        }

        if (authType == "key") {
            ExposedDropdownMenuBox(
                expanded = keyMenuExpanded,
                onExpandedChange = { keyMenuExpanded = it },
            ) {
                OutlinedTextField(
                    value = selectedKeyLabel,
                    onValueChange = {},
                    readOnly = true,
                    label = { Text("鍵") },
                    trailingIcon = {
                        ExposedDropdownMenuDefaults.TrailingIcon(expanded = keyMenuExpanded)
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .menuAnchor(MenuAnchorType.PrimaryNotEditable),
                )
                ExposedDropdownMenu(
                    expanded = keyMenuExpanded,
                    onDismissRequest = { keyMenuExpanded = false },
                ) {
                    if (keys.isEmpty()) {
                        DropdownMenuItem(
                            text = { Text("登録された鍵がありません") },
                            onClick = { keyMenuExpanded = false },
                        )
                    } else {
                        keys.forEach { key ->
                            DropdownMenuItem(
                                text = { Text(key.label) },
                                onClick = {
                                    keyId = key.id
                                    keyMenuExpanded = false
                                },
                            )
                        }
                    }
                }
            }
        }

        Spacer(Modifier.height(4.dp))

        Text("接続方式")
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.horizontalScroll(rememberScrollState()),
        ) {
            FilterChip(
                selected = transportPreference == TransportPreference.PLAIN_SSH,
                onClick = { transportPreference = TransportPreference.PLAIN_SSH },
                label = { Text("通常 SSH") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.TSSHD_QUIC,
                onClick = { transportPreference = TransportPreference.TSSHD_QUIC },
                label = { Text("tsshd QUIC") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.ISEKAI_HELPER_QUIC,
                onClick = { transportPreference = TransportPreference.ISEKAI_HELPER_QUIC },
                label = { Text("自作ヘルパー QUIC") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.AUTO,
                onClick = { transportPreference = TransportPreference.AUTO },
                label = { Text("Auto（推奨）") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH,
                onClick = { transportPreference = TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH },
                label = { Text("自作ヘルパー QUIC（マルチパス）") },
            )
        }

        if (transportPreference == TransportPreference.ISEKAI_HELPER_QUIC ||
            transportPreference == TransportPreference.AUTO ||
            transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH
        ) {
            Text(
                text = "初回接続時に SSH 経由で自作ヘルパー（isekai-helper）を自動配布・起動します" +
                    "（対応 OS: Linux x86_64 / aarch64）。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        if (transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH) {
            OutlinedTextField(
                value = directAddress,
                onValueChange = { directAddress = it },
                label = { Text("直接到達アドレス（path1、任意）") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                text = "上の「ホスト」欄（通常 Tailscale 経由アドレス）と、こちらの直接到達可能な" +
                    "アドレスの両方を同時に維持し、片方が不安定でも即座にもう片方へ切り替えます。" +
                    "未入力なら通常の自作ヘルパー QUIC と同じ動作になります。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(
                    checked = enablePhysicalMultipath,
                    onCheckedChange = { enablePhysicalMultipath = it },
                )
                Text("Wi-Fi/セルラー物理無線も同時に使う（実験的）")
            }
            Text(
                text = "Wi-Fiとセルラーの両方の無線を同時に使い、片方が不安定でも即座に" +
                    "もう片方へ切り替えます。Tailscale使用中はこの機能は効果がありません" +
                    "（OSの制約でTailscale稼働中は物理無線への明示的なバインドができないため、" +
                    "自動的に上の直接到達アドレスのみのマルチパスにフォールバックします）。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            if (enablePhysicalMultipath) {
                OutlinedTextField(
                    value = cellularRemoteAddress,
                    onValueChange = { cellularRemoteAddress = it },
                    label = { Text("セルラー用の別リモートアドレス（任意、実験的）") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Text(
                    text = "同一サーバーの別アドレス（例: IPv6）をセルラー経路専用に指定できます。" +
                        "未入力なら上の直接到達アドレスと同じものを使います。",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(
                    checked = enableUpstreamFailover,
                    onCheckedChange = { enableUpstreamFailover = it },
                )
                Text("WiFiのupstream断を検知したらセルラーへ切り替える（実験的）")
            }
            Text(
                text = "WiFiは繋がっているが実際にはインターネットに到達できない状態" +
                    "（カフェ等のキャプティブポータル、ルーター障害）を検知して、" +
                    "セルラーに明示的にバインドしたソケットへ通信を丸ごと切り替えます。" +
                    "Tailscale使用中はこの機能は効果がありません。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        if (transportPreference == TransportPreference.TSSHD_QUIC) {
            OutlinedTextField(
                value = tsshdPort,
                onValueChange = { new -> tsshdPort = new.filter { it.isDigit() }.take(5) },
                label = { Text("tsshd ポート") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.fillMaxWidth(),
            )
        }

        Spacer(Modifier.height(4.dp))

        OutlinedTextField(
            value = postConnectCommands,
            onValueChange = { postConnectCommands = it },
            label = { Text("接続後に自動実行するコマンド（改行区切りで複数可）") },
            modifier = Modifier
                .fillMaxWidth()
                .height(120.dp),
        )
        Text(
            "注意: パスワードなどの機密情報をここに平文で書くと、保護されずデータベースに残ります。",
            color = MaterialTheme.colorScheme.error,
            fontSize = 12.sp,
        )

        Spacer(Modifier.height(4.dp))

        Text("ポートフォワード", fontWeight = FontWeight.Bold)
        Text(
            "接続確立後、指定したローカルポートへの接続をリモートホストへ中継します(現状は -L のみ対応)。",
            fontSize = 12.sp,
        )

        forwardDrafts.forEachIndexed { index, draft ->
            Column(
                modifier = Modifier.fillMaxWidth(),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text("ローカルフォワード #${index + 1}(種別: ローカル -L)")
                    OutlinedButton(onClick = { forwardDrafts.removeAt(index) }) { Text("削除") }
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = draft.bindAddress,
                        onValueChange = { new -> forwardDrafts[index] = draft.copy(bindAddress = new) },
                        label = { Text("待受アドレス") },
                        singleLine = true,
                        modifier = Modifier.weight(1f),
                    )
                    OutlinedTextField(
                        value = draft.bindPort,
                        onValueChange = { new -> forwardDrafts[index] = draft.copy(bindPort = new.filter { it.isDigit() }.take(5)) },
                        label = { Text("待受ポート") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                        modifier = Modifier.weight(1f),
                    )
                }
                if (draft.bindAddress.isNotBlank() &&
                    draft.bindAddress != "127.0.0.1" && draft.bindAddress != "localhost"
                ) {
                    Text(
                        "⚠ 同一 Wi-Fi/LAN 上の第三者からアクセスされうる待受アドレスです。",
                        color = Color(0xFFB00020),
                        fontSize = 12.sp,
                    )
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = draft.remoteHost,
                        onValueChange = { new -> forwardDrafts[index] = draft.copy(remoteHost = new) },
                        label = { Text("転送先ホスト") },
                        singleLine = true,
                        modifier = Modifier.weight(1f),
                    )
                    OutlinedTextField(
                        value = draft.remotePort,
                        onValueChange = { new -> forwardDrafts[index] = draft.copy(remotePort = new.filter { it.isDigit() }.take(5)) },
                        label = { Text("転送先ポート") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                        modifier = Modifier.weight(1f),
                    )
                }
            }
        }

        OutlinedButton(onClick = { forwardDrafts.add(ForwardDraft()) }) {
            Text("+ ポートフォワードを追加")
        }

        Spacer(Modifier.height(8.dp))

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = {
                    val saved = ConnectionProfile(
                        id = profile?.id ?: 0,
                        label = label.trim(),
                        host = host.trim(),
                        port = port.toIntOrNull() ?: 22,
                        username = username.trim(),
                        authType = authType,
                        keyId = if (authType == "key") keyId else null,
                        sortOrder = profile?.sortOrder ?: 0,
                        useTsshd = transportPreference == TransportPreference.TSSHD_QUIC,
                        tsshdPort = tsshdPort.toIntOrNull() ?: ConnectionProfile.DEFAULT_TSSHD_PORT,
                        transportPreferenceName = transportPreference.name,
                        directAddress = directAddress.trim().takeIf { it.isNotBlank() },
                        enablePhysicalMultipath = enablePhysicalMultipath,
                        cellularRemoteAddress = cellularRemoteAddress.trim().takeIf { it.isNotBlank() },
                        enableUpstreamFailover = enableUpstreamFailover,
                        postConnectCommands = postConnectCommands.trim().takeIf { it.isNotEmpty() },
                        forwards = forwardDrafts.mapNotNull { it.toPortForwardOrNull() },
                    )
                    vm.save(saved) { onSave() }
                },
                enabled = canSave && !isSaving,
            ) { Text("保存") }
            OutlinedButton(onClick = {
                RemoteLogger.i("TsshProfile", "cancelled profile edit (${if (profile == null) "new" else "id=${profile.id} '${profile.label}'"})")
                onCancel()
            }) { Text("キャンセル") }
        }
    }
}
