package com.example.imespike

import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.IBinder
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import com.example.imespike.data.ConnectionProfile
import com.example.imespike.session.AuthValidation
import com.example.imespike.session.AuthValidator
import com.example.imespike.session.DefaultSshGateway
import com.example.imespike.session.HostKeyChecker
import com.example.imespike.session.RealHostKeyChecker
import com.example.imespike.session.TerminalSession
import com.example.imespike.spike.KeystoreKek
import com.example.imespike.util.RemoteLogger
import uniffi.tssh_core.*
import java.io.File

data class TerminalUiState(
    val connected: Boolean = false,
    val statusMsg: String = "未接続",
    val screenUpdate: ScreenUpdate? = null,
    val lastFingerprint: String? = null,
    val scrollbackLen: Int = 0,
    val currentHost: String? = null,
    val hostKeyChangedWarning: HostKeyChangedWarning? = null,
    val trzszState: TrzszUiState? = null,
)

sealed class TrzszUiState {
    data class WaitingUser(
        val transferId: String,
        val mode: String,
        val suggestedName: String?,
        val expectedSize: ULong?,
    ) : TrzszUiState()

    data class InProgress(
        val transferId: String,
        val mode: String,
        val fileName: String?,
        val transferred: ULong,
        val total: ULong?,
    ) : TrzszUiState()

    data class Done(
        val transferId: String,
        val success: Boolean,
        val message: String?,
    ) : TrzszUiState()
}

data class HostKeyChangedWarning(
    val host: String,
    val port: Int,
    val oldFingerprint: String,
    val newFingerprint: String,
)

/**
 * Android ライフサイクル専任の薄いラッパー。
 * ドメインロジックはすべて [TerminalSession] に委譲。
 * 担当: Service バインド、NetworkCallback、認証解決（Keystore/Room アクセス）。
 */
class TerminalViewModel(
    app: Application,
    internal val session: TerminalSession,
) : AndroidViewModel(app) {

    /** 本番用コンストラクタ。Compose の viewModel() から呼ばれる。 */
    constructor(app: Application) : this(
        app,
        TerminalSession(DefaultSshGateway(), RealHostKeyChecker(app)),
    )

    val uiState: StateFlow<TerminalUiState> = session.state
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = session.pendingDownloadFile

    @Volatile private var terminalService: TerminalSessionService? = null
    private var isServiceBound = false

    private val serviceConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val svc = (binder as TerminalSessionService.SessionBinder).getService()
            terminalService = svc
            RemoteLogger.i("TsshVM", "service bound OK")
        }
        override fun onServiceDisconnected(name: ComponentName) {
            RemoteLogger.w("TsshVM", "service disconnected unexpectedly")
            terminalService = null
        }
    }

    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            RemoteLogger.i("TsshSSH", "network available")
        }
        override fun onLost(network: Network) {
            if (uiState.value.connected) {
                RemoteLogger.w("TsshSSH", "network lost while connected")
                session.disconnect()
            }
        }
    }

    init {
        val a = getApplication<Application>()
        isServiceBound = a.bindService(Intent(a, TerminalSessionService::class.java), serviceConnection, 0)
        RemoteLogger.i("TsshVM", "TerminalViewModel created (serviceBound=$isServiceBound)")

        val cm = a.getSystemService(ConnectivityManager::class.java)
        cm.registerNetworkCallback(
            NetworkRequest.Builder().addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET).build(),
            networkCallback,
        )

        // Service 通知を state 変化から自動駆動
        viewModelScope.launch {
            var prevConnected = false
            uiState.collect { state ->
                val connected = state.connected
                if (connected && !prevConnected) {
                    terminalService?.notifyConnected(state.currentHost ?: "")
                } else if (!connected && prevConnected) {
                    terminalService?.notifyDisconnected()
                }
                prevConnected = connected
            }
        }
    }

    // ── 接続 ─────────────────────────────────────────────────────

    fun connect(config: SshConfig) {
        if (uiState.value.connected) return
        RemoteLogger.i("TsshSSH", "connect: ${config.username}@${config.host}:${config.port}")
        val a = getApplication<Application>()
        a.startService(Intent(a, TerminalSessionService::class.java))
        if (!isServiceBound) {
            isServiceBound = a.bindService(
                Intent(a, TerminalSessionService::class.java), serviceConnection, Context.BIND_AUTO_CREATE
            )
        }
        session.connect(config)
    }

    fun connectProfile(profile: ConnectionProfile, password: String? = null) {
        if (uiState.value.connected) return
        RemoteLogger.i("TsshSSH", "connectProfile: '${profile.label}' ${profile.username}@${profile.host}:${profile.port}")
        viewModelScope.launch(Dispatchers.IO) {
            val auth = resolveAuth(profile, password) ?: return@launch
            val config = SshConfig(
                host = profile.host,
                port = profile.port.toUShort(),
                username = profile.username,
                auth = auth,
                cols = 80u,
                rows = 24u,
            )
            connect(config)
        }
    }

    private suspend fun resolveAuth(profile: ConnectionProfile, password: String?): SshAuth? {
        return when (val v = AuthValidator.validate(profile.authType, password, profile.keyId)) {
            is AuthValidation.Error -> {
                RemoteLogger.w("TsshSSH", "auth error: ${v.statusMsg}")
                session.notifyAuthError(v.statusMsg)
                null
            }
            is AuthValidation.Password -> {
                RemoteLogger.i("TsshSSH", "auth: password")
                SshAuth.Password(v.value)
            }
            is AuthValidation.PublicKey -> {
                RemoteLogger.i("TsshSSH", "auth: public key id=${v.keyId}")
                loadPublicKeyAuth(v.keyId)
            }
        }
    }

    private suspend fun loadPublicKeyAuth(keyId: Long): SshAuth? =
        runCatching {
            val keyEntry = com.example.imespike.data.Repositories.keys.findById(keyId)
                ?: error("鍵が見つかりません (id=$keyId)")
            RemoteLogger.i("TsshSSH", "decrypting key '${keyEntry.label}'")
            val encBytes = File(keyEntry.encryptedPrivateKeyPath).readBytes()
            val pem = KeystoreKek.decrypt(encBytes)
            RemoteLogger.i("TsshSSH", "key decrypted OK (${pem.size} bytes)")
            SshAuth.PublicKey(pem)
        }.getOrElse { e ->
            RemoteLogger.e("TsshSSH", "key error: ${e.message}", e)
            session.notifyAuthError("鍵エラー: ${e.message}")
            null
        }

    // ── セッション操作（すべて session 委譲）──────────────────────

    fun send(bytes: ByteArray) = session.send(bytes)
    fun resize(cols: UInt, rows: UInt) {
        RemoteLogger.d("TsshSSH", "resize → ${cols}×${rows}")
        session.resize(cols, rows)
    }
    fun disconnect() = session.disconnect()
    fun scrollbackCells(offset: Int, rows: Int) = session.scrollbackCells(offset, rows)

    fun trustUpdatedHostKey() = session.trustUpdatedHostKey()
    fun dismissHostKeyWarning() = session.dismissHostKeyWarning()
    fun consumeDownloadFile() = session.consumeDownloadFile()

    fun getSessionLog(): String = session.log.value
    fun clearSessionLog() = session.clearLog()

    // ── trzsz（Android ファイル I/O はここで処理）────────────────

    fun trzszStartUpload(uri: android.net.Uri, context: Context) {
        val state = uiState.value.trzszState as? TrzszUiState.WaitingUser ?: return
        val transferId = state.transferId
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val cr = context.contentResolver
                val fileName = getFileName(cr, uri) ?: "file"
                val fileSize = getFileSize(cr, uri) ?: 0L
                RemoteLogger.i("TrzszUpload", "start fileName=$fileName fileSize=$fileSize")
                // WaitingUser → InProgress は最初の onTrzszProgress イベントで自動遷移
                session.trzszAcceptUpload(transferId, fileName, fileSize.toULong(), 0u)
                val chunkSize = 64 * 1024
                cr.openInputStream(uri)?.use { inp ->
                    val buf = ByteArray(chunkSize)
                    var pending: ByteArray? = null
                    var chunkCount = 0
                    while (true) {
                        val n = inp.read(buf)
                        if (n == -1) {
                            RemoteLogger.i("TrzszUpload", "last chunk, total=$chunkCount")
                            session.trzszSendChunk(transferId, pending ?: ByteArray(0), true)
                            break
                        }
                        pending?.let { session.trzszSendChunk(transferId, it, false) }
                        pending = buf.copyOf(n)
                        chunkCount++
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("TrzszUpload", "exception: $e")
            }
        }
    }

    fun trzszStartDownload() {
        val state = uiState.value.trzszState as? TrzszUiState.WaitingUser ?: return
        session.trzszAcceptDownload(state.transferId)
        // InProgress への状態遷移はサーバからの onTrzszProgress で自動的に起こる
    }

    fun trzszCancel() {
        val tid = when (val s = uiState.value.trzszState) {
            is TrzszUiState.WaitingUser -> s.transferId
            is TrzszUiState.InProgress  -> s.transferId
            else -> null
        } ?: return
        session.trzszCancel(tid)
        session.trzszDismiss()
    }

    fun trzszDismiss() = session.trzszDismiss()

    // ── ユーティリティ ────────────────────────────────────────────

    private fun getFileName(cr: android.content.ContentResolver, uri: android.net.Uri): String? {
        cr.query(uri, null, null, null, null)?.use { cursor ->
            val idx = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
            if (cursor.moveToFirst() && idx >= 0) return cursor.getString(idx)
        }
        return uri.lastPathSegment
    }

    private fun getFileSize(cr: android.content.ContentResolver, uri: android.net.Uri): Long? {
        cr.query(uri, null, null, null, null)?.use { cursor ->
            val idx = cursor.getColumnIndex(android.provider.OpenableColumns.SIZE)
            if (cursor.moveToFirst() && idx >= 0) return cursor.getLong(idx)
        }
        return null
    }

    override fun onCleared() {
        super.onCleared()
        RemoteLogger.i("TsshVM", "TerminalViewModel cleared")
        session.close()
        val cm = getApplication<Application>().getSystemService(ConnectivityManager::class.java)
        try { cm.unregisterNetworkCallback(networkCallback) } catch (_: Exception) {}
        if (isServiceBound) {
            getApplication<Application>().unbindService(serviceConnection)
            isServiceBound = false
        }
    }
}
