package com.example.imespike

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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.example.imespike.data.ConnectionProfile
import com.example.imespike.data.KeyEntry
import com.example.imespike.data.Repositories
import com.example.imespike.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileEditScreen(
    profile: ConnectionProfile? = null,
    onSave: () -> Unit,
    onCancel: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val keys = remember { mutableStateListOf<KeyEntry>() }

    var label by remember { mutableStateOf(profile?.label ?: "") }
    var host by remember { mutableStateOf(profile?.host ?: "") }
    var port by remember { mutableStateOf((profile?.port ?: 22).toString()) }
    var username by remember { mutableStateOf(profile?.username ?: "") }
    var authType by remember { mutableStateOf(profile?.authType ?: "password") }
    var keyId by remember { mutableStateOf(profile?.keyId) }
    var keyMenuExpanded by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) {
        val list = withContext(Dispatchers.IO) { Repositories.keys.getAll() }
        keys.clear()
        keys.addAll(list)
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
                        .menuAnchor(),
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
                        useMosh = profile?.useMosh ?: false,
                        sortOrder = profile?.sortOrder ?: 0,
                    )
                    scope.launch {
                        RemoteLogger.i("TsshProfile", "saving profile: label='${saved.label}' host=${saved.host}:${saved.port} user=${saved.username} authType=${saved.authType} keyId=${saved.keyId} id=${if (saved.id == 0L) "new" else "${saved.id}"}")
                        withContext(Dispatchers.IO) { Repositories.profiles.save(saved) }
                        onSave()
                    }
                },
                enabled = canSave,
            ) { Text("保存") }
            OutlinedButton(onClick = {
                RemoteLogger.i("TsshProfile", "cancelled profile edit (${if (profile == null) "new" else "id=${profile.id} '${profile.label}'"})")
                onCancel()
            }) { Text("キャンセル") }
        }
    }
}
