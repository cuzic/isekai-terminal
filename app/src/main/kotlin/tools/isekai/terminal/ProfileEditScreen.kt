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
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.ForwardType
import uniffi.isekai_terminal_core.PortForward
import uniffi.isekai_terminal_core.TransportPreference

/**
 * ポートフォワード編集欄用の入力中ドラフト([PortForward] は bindPort/remotePort が
 * UShort で空文字を表現できないため、テキスト入力中は String で持つ)。
 */
private data class ForwardDraft(
    val forwardType: ForwardType = ForwardType.LOCAL,
    val bindAddress: String = "127.0.0.1",
    val bindPort: String = "",
    val remoteHost: String = "",
    val remotePort: String = "",
)

private fun PortForward.toDraft() = ForwardDraft(
    forwardType = forwardType,
    bindAddress = bindAddress,
    bindPort = bindPort.toString(),
    remoteHost = remoteHost,
    remotePort = remotePort.toString(),
)

/**
 * Local/Remoteはremote_host/remote_portが必須(前者は転送先、後者はクライアントから見た
 * ローカルターゲット)。Dynamic(SOCKS)は宛先が接続ごとに動的に決まるため両方不要。
 * 不正な行は保存対象から除外する。
 */
private fun ForwardDraft.toPortForwardOrNull(): PortForward? {
    val bp = bindPort.toIntOrNull() ?: return null
    if (bp !in 1..65535) return null
    return when (forwardType) {
        ForwardType.DYNAMIC -> PortForward(
            forwardType = ForwardType.DYNAMIC,
            bindAddress = bindAddress.ifBlank { "127.0.0.1" },
            bindPort = bp.toUShort(),
            remoteHost = "",
            remotePort = 0u,
        )
        ForwardType.LOCAL, ForwardType.REMOTE -> {
            val rp = remotePort.toIntOrNull() ?: return null
            if (remoteHost.isBlank() || rp !in 1..65535) return null
            PortForward(
                forwardType = forwardType,
                bindAddress = bindAddress.ifBlank { "127.0.0.1" },
                bindPort = bp.toUShort(),
                remoteHost = remoteHost,
                remotePort = rp.toUShort(),
            )
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileEditScreen(
    profile: ConnectionProfile? = null,
    onSave: () -> Unit,
    onCancel: () -> Unit,
    // relayJwt は Room に暗号化して保存する(issue #1)。AndroidKeyStore は Robolectric で
    // 使えないため、実際の暗号化処理はデフォルト引数として注入し、テストでは恒等関数
    // ({ it })に差し替える(ProfileListScreen の applyTerminalTheme と同じパターン)。
    encryptRelayJwt: (String) -> String = RelayCredentialVault::encrypt,
    decryptRelayJwt: (String) -> String = RelayCredentialVault::decrypt,
) {
    val vm: ProfileEditViewModel = viewModel()
    val keys by vm.keys.collectAsStateWithLifecycle()
    val isSaving by vm.isSaving.collectAsStateWithLifecycle()

    // Phase 12 P2-1: per-session/per-hostのterminal theme。null="グローバル既定に従う"。
    var themeName by remember { mutableStateOf(profile?.themeName) }
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
    var helperBindPort by remember { mutableStateOf(profile?.helperBindPort?.toString() ?: "") }
    var stunServer by remember { mutableStateOf(profile?.stunServer ?: "") }
    var relayAddr by remember { mutableStateOf(profile?.relayAddr ?: "") }
    var relaySni by remember { mutableStateOf(profile?.relaySni ?: "") }
    var relayJwt by remember { mutableStateOf(profile?.relayJwt?.let(decryptRelayJwt) ?: "") }
    var enableUpstreamFailover by remember { mutableStateOf(profile?.enableUpstreamFailover ?: false) }
    var postConnectCommands by remember { mutableStateOf(profile?.postConnectCommands ?: "") }
    var enableAgentForward by remember { mutableStateOf(profile?.enableAgentForward ?: false) }
    var allowNonLoopbackForwardBind by remember { mutableStateOf(profile?.allowNonLoopbackForwardBind ?: false) }
    var useJumpHost by remember { mutableStateOf(profile?.usesJumpHost ?: false) }
    var jumpHost by remember { mutableStateOf(profile?.jumpHost ?: "") }
    var jumpPort by remember { mutableStateOf((profile?.jumpPort ?: 22).toString()) }
    var jumpUsername by remember { mutableStateOf(profile?.jumpUsername ?: "") }
    var jumpAuthType by remember { mutableStateOf(profile?.jumpAuthType ?: "password") }
    var jumpKeyId by remember { mutableStateOf(profile?.jumpKeyId) }
    var jumpKeyMenuExpanded by remember { mutableStateOf(false) }
    val forwardDrafts = remember {
        mutableStateListOf<ForwardDraft>().apply {
            profile?.forwards?.forEach { add(it.toDraft()) }
        }
    }

    val selectedKeyLabel = keys.firstOrNull { it.id == keyId }?.label ?: "鍵を選択"
    val selectedJumpKeyLabel = keys.firstOrNull { it.id == jumpKeyId }?.label ?: "鍵を選択"
    // 1024未満は多くの環境でサーバー側の管理者権限(CAP_NET_BIND_SERVICE)が必要になるため、
    // ユーザーが誤って指定しないよう非特権ポート範囲のみ許可する(空欄はこれまで通りOK)。
    val helperBindPortValid = helperBindPort.isBlank() || (helperBindPort.toIntOrNull()?.let { it in 1024..65535 } ?: false)
    val canSave = label.isNotBlank() && host.isNotBlank() && username.isNotBlank() &&
        (authType == "password" || keyId != null) &&
        (!useJumpHost || (
            jumpHost.isNotBlank() && jumpUsername.isNotBlank() &&
                (jumpAuthType == "password" || jumpKeyId != null)
        )) &&
        (transportPreference != TransportPreference.ISEKAI_LINK_RELAY_QUIC || (
            relayAddr.isNotBlank() && relaySni.isNotBlank() && relayJwt.isNotBlank()
        )) &&
        helperBindPortValid

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
            modifier = Modifier.fillMaxWidth().testTag("profileLabelField"),
        )
        OutlinedTextField(
            value = host,
            onValueChange = { host = it },
            label = { Text("ホスト") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth().testTag("profileHostField"),
        )
        OutlinedTextField(
            value = port,
            onValueChange = { new -> port = new.filter { it.isDigit() }.take(5) },
            label = { Text("ポート") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            modifier = Modifier.fillMaxWidth().testTag("profilePortField"),
        )
        OutlinedTextField(
            value = username,
            onValueChange = { username = it },
            label = { Text("ユーザー名") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth().testTag("profileUsernameField"),
        )

        Text("認証方式")
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(
                selected = authType == "password",
                onClick = { authType = "password" },
                label = { Text("パスワード") },
                modifier = Modifier.testTag("authTypePasswordChip"),
            )
            FilterChip(
                selected = authType == "key",
                onClick = { authType = "key" },
                label = { Text("鍵認証") },
                modifier = Modifier.testTag("authTypeKeyChip"),
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
                        .menuAnchor(MenuAnchorType.PrimaryNotEditable)
                        .testTag("profileKeyDropdownField"),
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

        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(
                checked = useJumpHost,
                onCheckedChange = { useJumpHost = it },
                modifier = Modifier.testTag("useJumpHostCheckbox"),
            )
            Text("踏み台(ProxyJump)経由で接続する")
        }
        Text(
            text = "上の「ホスト」へ直接到達できない場合、まずこの踏み台ホストへSSH接続してから" +
                "トンネルします(ssh -J 相当)。「tsshd QUIC」接続方式以外の全方式で有効です。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        if (useJumpHost) {
            OutlinedTextField(
                value = jumpHost,
                onValueChange = { jumpHost = it },
                label = { Text("踏み台ホスト") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = jumpPort,
                onValueChange = { new -> jumpPort = new.filter { it.isDigit() }.take(5) },
                label = { Text("踏み台ポート") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = jumpUsername,
                onValueChange = { jumpUsername = it },
                label = { Text("踏み台ユーザー名") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )

            Text("踏み台の認証方式")
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = jumpAuthType == "password",
                    onClick = { jumpAuthType = "password" },
                    label = { Text("パスワード") },
                )
                FilterChip(
                    selected = jumpAuthType == "key",
                    onClick = { jumpAuthType = "key" },
                    label = { Text("鍵認証") },
                )
            }

            if (jumpAuthType == "key") {
                ExposedDropdownMenuBox(
                    expanded = jumpKeyMenuExpanded,
                    onExpandedChange = { jumpKeyMenuExpanded = it },
                ) {
                    OutlinedTextField(
                        value = selectedJumpKeyLabel,
                        onValueChange = {},
                        readOnly = true,
                        label = { Text("踏み台の鍵") },
                        trailingIcon = {
                            ExposedDropdownMenuDefaults.TrailingIcon(expanded = jumpKeyMenuExpanded)
                        },
                        modifier = Modifier
                            .fillMaxWidth()
                            .menuAnchor(MenuAnchorType.PrimaryNotEditable),
                    )
                    ExposedDropdownMenu(
                        expanded = jumpKeyMenuExpanded,
                        onDismissRequest = { jumpKeyMenuExpanded = false },
                    ) {
                        if (keys.isEmpty()) {
                            DropdownMenuItem(
                                text = { Text("登録された鍵がありません") },
                                onClick = { jumpKeyMenuExpanded = false },
                            )
                        } else {
                            keys.forEach { key ->
                                DropdownMenuItem(
                                    text = { Text(key.label) },
                                    onClick = {
                                        jumpKeyId = key.id
                                        jumpKeyMenuExpanded = false
                                    },
                                )
                            }
                        }
                    }
                }
            }
        }

        Spacer(Modifier.height(4.dp))

        Text("接続方式")
        Text(
            text = "以下の3つは「失敗時どうなるか」が異なる接続ポリシーです。うまく繋がらない場合は" +
                "上のポリシーへ切り替えてください。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        Spacer(Modifier.height(4.dp))
        Text("Plain SSH", fontWeight = FontWeight.Bold, fontSize = 13.sp)
        Text(
            text = "NAT越え・P2Pは一切行わない従来のSSH接続。フォールバックという概念自体が無い基本方式です。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
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
        }

        Spacer(Modifier.height(8.dp))
        Text("Smart Connect（推奨）", fontWeight = FontWeight.Bold, fontSize = 13.sp)
        Text(
            text = "自作ヘルパー経由QUICを試し、失敗したら自動的に通常SSHへフォールバックします。" +
                "迷ったらこれを選んでください。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.horizontalScroll(rememberScrollState()),
        ) {
            FilterChip(
                selected = transportPreference == TransportPreference.AUTO,
                onClick = { transportPreference = TransportPreference.AUTO },
                label = { Text("Auto（推奨）") },
            )
        }

        Spacer(Modifier.height(8.dp))
        Text("Strict Isekai Link（実験的・フォールバックなし）", fontWeight = FontWeight.Bold, fontSize = 13.sp)
        Text(
            text = "指定した経路のみを使用します。穴あけ・ヘルパー起動などが失敗しても自動フォールバック" +
                "せず、その場で接続エラーになります（経路そのものが信頼境界のため、意図せず別経路へ" +
                "落ちないようにしています）。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.horizontalScroll(rememberScrollState()),
        ) {
            FilterChip(
                selected = transportPreference == TransportPreference.ISEKAI_HELPER_QUIC,
                onClick = { transportPreference = TransportPreference.ISEKAI_HELPER_QUIC },
                label = { Text("自作ヘルパー QUIC") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH,
                onClick = { transportPreference = TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH },
                label = { Text("自作ヘルパー QUIC（マルチパス）") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.ISEKAI_STUN_P2P_QUIC,
                onClick = { transportPreference = TransportPreference.ISEKAI_STUN_P2P_QUIC },
                label = { Text("STUN P2P QUIC（実験的）") },
            )
            FilterChip(
                selected = transportPreference == TransportPreference.ISEKAI_LINK_RELAY_QUIC,
                onClick = { transportPreference = TransportPreference.ISEKAI_LINK_RELAY_QUIC },
                label = { Text("relay P2P QUIC（実験的）") },
            )
        }

        if (transportPreference == TransportPreference.ISEKAI_HELPER_QUIC ||
            transportPreference == TransportPreference.AUTO ||
            transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH ||
            transportPreference == TransportPreference.ISEKAI_STUN_P2P_QUIC ||
            transportPreference == TransportPreference.ISEKAI_LINK_RELAY_QUIC
        ) {
            Text(
                text = "初回接続時に SSH 経由で自作ヘルパー（isekai-helper）を自動配布・起動します" +
                    "（対応 OS: Linux x86_64 / aarch64）。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        if (transportPreference == TransportPreference.ISEKAI_HELPER_QUIC ||
            transportPreference == TransportPreference.AUTO ||
            transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH
        ) {
            OutlinedTextField(
                value = helperBindPort,
                onValueChange = { new -> helperBindPort = new.filter { it.isDigit() }.take(5) },
                label = { Text("ヘルパー待受ポート固定（任意、1024〜65535）") },
                placeholder = { Text("未指定なら自動割り当て") },
                singleLine = true,
                isError = !helperBindPortValid,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                text = "自作ヘルパーのQUIC待受ポートを固定します。サーバーへ直接到達する経路" +
                    "（direct_address等）を使う場合、サーバー側ファイアウォールで事前にこの" +
                    "ポートだけを開けておけます（未指定ならこれまで通り自動割り当てのため、" +
                    "接続前にポート番号が分からずファイアウォール許可ができません）。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (transportPreference == TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH &&
                directAddress.isNotBlank() && helperBindPort.isBlank()
            ) {
                Text(
                    text = "直接到達アドレス(direct_address)が設定されているため、このまま未指定でも" +
                        "既定の固定ポート45823が使われます(完全なエフェメラルにはなりません)。",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (!helperBindPortValid) {
                Text(
                    text = "⚠ 1024〜65535の範囲で指定してください" +
                        "（1024未満は多くの環境でサーバー側の管理者権限が必要です）。",
                    color = Color(0xFFB00020),
                    fontSize = 12.sp,
                )
            }
        }

        if (transportPreference == TransportPreference.ISEKAI_STUN_P2P_QUIC) {
            OutlinedTextField(
                value = stunServer,
                onValueChange = { stunServer = it },
                label = { Text("STUNサーバー（任意）") },
                placeholder = { Text(ConnectionProfile.DEFAULT_STUN_SERVER) },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                text = "relay サーバーを一切使わず、STUN でお互いの外部アドレスを調べて直接 " +
                    "UDP 穴あけ（simultaneous open）を試みます。NAT の種類によっては穴あけが " +
                    "成立せず接続に失敗することがあり、その場合はフォールバックせずエラーになります" +
                    "（その際は Auto 等、他の接続方式に切り替えてください）。未入力なら " +
                    "${ConnectionProfile.DEFAULT_STUN_SERVER} を使います。",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        if (transportPreference == TransportPreference.ISEKAI_LINK_RELAY_QUIC) {
            OutlinedTextField(
                value = relayAddr,
                onValueChange = { relayAddr = it },
                label = { Text("relayアドレス（host:port）") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = relaySni,
                onValueChange = { relaySni = it },
                label = { Text("relay SNI") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = relayJwt,
                onValueChange = { relayJwt = it },
                label = { Text("relay JWT") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                text = "MASQUE relay(bound-udp-server)経由で常時到達可能なP2P QUIC接続を行います。" +
                    "relayが常に経路に残るためNATの種類に依存しませんが、relayアドレス・SNI・JWTの" +
                    "3つ全てが必要です（JWTの発行・配布フローは別途用意する運用を想定、PLAN.md参照）。",
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

            // Phase 9-4: noq #738により常時no-opなので、一般ユーザー向けリリースビルドでは
            // 非表示にする(experimental feature flag、外部レビュー指摘対応)。debugビルドでは
            // 開発・実機検証のため引き続き表示する。
            if (BuildConfig.ENABLE_EXPERIMENTAL_PHYSICAL_MULTIPATH) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(
                        checked = enablePhysicalMultipath,
                        onCheckedChange = { enablePhysicalMultipath = it },
                    )
                    Text("Wi-Fi/セルラー物理無線への同時マルチパス（現在利用不可・開発者向け）")
                }
                Text(
                    text = "状態: 現在利用不可。原因: noq側の既知バグ" +
                        "（open_path()にlocal_ip明示指定した経路でPATH_RESPONSEが届かずvalidation " +
                        "failedになる、noq issue #738、Needs Triage）。フォールバック: ONにしても" +
                        "実際には物理無線への同時バインドは行われず、上の「直接到達アドレス」欄による" +
                        "Tailscale⇔直接アドレスのマルチパスのみが有効なままです（日和見的フォールバック、" +
                        "黙って無効化されるだけでエラーにはなりません）。noq側の修正が入り次第有効化予定です。" +
                        "Tailscale使用中はさらにOSの制約で物理無線への明示的なバインドができません。",
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

        Text("配色テーマ", fontWeight = FontWeight.Bold)
        Text(
            "未指定ならアプリ全体の既定テーマに従います（ホームの「配色」設定）。" +
                "このプロファイルで接続したタブは、後からタブ側で個別に上書きすることもできます。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.horizontalScroll(rememberScrollState()),
        ) {
            FilterChip(
                selected = themeName == null,
                onClick = { themeName = null },
                label = { Text("既定に従う") },
            )
            TerminalThemes.ALL.forEach { theme ->
                FilterChip(
                    selected = themeName == theme.name,
                    onClick = { themeName = theme.name },
                    label = { Text(theme.name) },
                )
            }
        }

        Spacer(Modifier.height(4.dp))

        Text("ポートフォワード", fontWeight = FontWeight.Bold)
        Text(
            "接続確立後にトンネルを張ります(plain SSHトランスポートのみ対応、QUIC系は非対応)。" +
                "Local(-L)はこの端末のポートへの接続をリモートへ、Remote(-R)はSSHサーバー側の" +
                "ポートへの接続をこの端末のローカルターゲットへ、Dynamic(-D)はSOCKS4/5プロキシ" +
                "として動的な宛先への接続を、それぞれ中継します。",
            fontSize = 12.sp,
        )
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(
                checked = allowNonLoopbackForwardBind,
                onCheckedChange = { allowNonLoopbackForwardBind = it },
                modifier = Modifier.testTag("allowNonLoopbackForwardBindCheckbox"),
            )
            Text("同一Wi-Fi/LAN上の他端末からの待受を許可する（非ループバックbind）")
        }
        Text(
            "OFF（既定）の場合、待受アドレスが 127.0.0.1/localhost 以外だとコア側で待受自体が" +
                "拒否されます（UI警告だけでなく実際に強制されます）。ONにする場合は、同一LAN上の" +
                "第三者からもアクセスされうることを理解した上で有効にしてください。",
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
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
                    Text("フォワード #${index + 1}")
                    OutlinedButton(onClick = { forwardDrafts.removeAt(index) }) { Text("削除") }
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    FilterChip(
                        selected = draft.forwardType == ForwardType.LOCAL,
                        onClick = { forwardDrafts[index] = draft.copy(forwardType = ForwardType.LOCAL) },
                        label = { Text("Local (-L)") },
                    )
                    FilterChip(
                        selected = draft.forwardType == ForwardType.REMOTE,
                        onClick = { forwardDrafts[index] = draft.copy(forwardType = ForwardType.REMOTE) },
                        label = { Text("Remote (-R)") },
                    )
                    FilterChip(
                        selected = draft.forwardType == ForwardType.DYNAMIC,
                        onClick = { forwardDrafts[index] = draft.copy(forwardType = ForwardType.DYNAMIC) },
                        label = { Text("Dynamic/SOCKS (-D)") },
                    )
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = draft.bindAddress,
                        onValueChange = { new -> forwardDrafts[index] = draft.copy(bindAddress = new) },
                        label = {
                            Text(if (draft.forwardType == ForwardType.REMOTE) "待受アドレス(SSHサーバー側)" else "待受アドレス")
                        },
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
                        text = if (allowNonLoopbackForwardBind) {
                            "⚠ 同一 Wi-Fi/LAN 上の第三者からアクセスされうる待受アドレスです。"
                        } else {
                            "⚠ このアドレスは上の「非ループバックbind」チェックがOFFのため、" +
                                "接続時にコア側で待受が拒否されます。"
                        },
                        color = Color(0xFFB00020),
                        fontSize = 12.sp,
                    )
                }
                if (draft.forwardType != ForwardType.DYNAMIC) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedTextField(
                            value = draft.remoteHost,
                            onValueChange = { new -> forwardDrafts[index] = draft.copy(remoteHost = new) },
                            label = {
                                Text(
                                    if (draft.forwardType == ForwardType.REMOTE) "ローカルターゲットホスト" else "転送先ホスト"
                                )
                            },
                            singleLine = true,
                            modifier = Modifier.weight(1f),
                        )
                        OutlinedTextField(
                            value = draft.remotePort,
                            onValueChange = { new -> forwardDrafts[index] = draft.copy(remotePort = new.filter { it.isDigit() }.take(5)) },
                            label = {
                                Text(
                                    if (draft.forwardType == ForwardType.REMOTE) "ローカルターゲットポート" else "転送先ポート"
                                )
                            },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier = Modifier.weight(1f),
                        )
                    }
                } else {
                    Text(
                        "SOCKS4/4a/5クライアントとして動作します。宛先は接続ごとにSOCKS" +
                            "ハンドシェイクで決まるため、転送先の指定は不要です。",
                        fontSize = 12.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }

        OutlinedButton(onClick = { forwardDrafts.add(ForwardDraft()) }) {
            Text("+ ポートフォワードを追加")
        }

        Spacer(Modifier.height(4.dp))

        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Text("SSH agent forwarding", modifier = Modifier.align(Alignment.CenterVertically))
                Switch(
                    checked = enableAgentForward,
                    onCheckedChange = { enableAgentForward = it },
                    modifier = Modifier.testTag("agentForwardSwitch"),
                )
            }
            if (enableAgentForward) {
                Text(
                    "有効にすると接続先サーバーの管理者や同居プロセスがあなたの秘密鍵での署名を要求できます。" +
                        "信頼できるホストのみで有効にしてください。署名要求ごとに確認ダイアログが表示されます。",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.error,
                )
            }
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
                        helperBindPort = helperBindPort.toIntOrNull()?.takeIf { it in 1024..65535 },
                        enableUpstreamFailover = enableUpstreamFailover,
                        postConnectCommands = postConnectCommands.trim().takeIf { it.isNotEmpty() },
                        forwards = forwardDrafts.mapNotNull { it.toPortForwardOrNull() },
                        allowNonLoopbackForwardBind = allowNonLoopbackForwardBind,
                        themeName = themeName,
                        enableAgentForward = enableAgentForward,
                        jumpHost = if (useJumpHost) jumpHost.trim() else null,
                        jumpPort = jumpPort.toIntOrNull() ?: 22,
                        jumpUsername = if (useJumpHost) jumpUsername.trim() else null,
                        jumpAuthType = if (useJumpHost) jumpAuthType else null,
                        jumpKeyId = if (useJumpHost && jumpAuthType == "key") jumpKeyId else null,
                        stunServer = stunServer.trim().takeIf { it.isNotBlank() },
                        relayAddr = relayAddr.trim().takeIf { it.isNotBlank() },
                        relaySni = relaySni.trim().takeIf { it.isNotBlank() },
                        relayJwt = relayJwt.trim().takeIf { it.isNotBlank() }?.let(encryptRelayJwt),
                    )
                    vm.save(saved) { onSave() }
                },
                enabled = canSave && !isSaving,
                modifier = Modifier.testTag("profileSaveButton"),
            ) { Text("保存") }
            OutlinedButton(
                onClick = {
                    RemoteLogger.i("IsekaiTerminalProfile", "cancelled profile edit (${if (profile == null) "new" else "id=${profile.id} '${profile.label}'"})")
                    onCancel()
                },
                modifier = Modifier.testTag("profileCancelButton"),
            ) { Text("キャンセル") }
        }
    }
}
