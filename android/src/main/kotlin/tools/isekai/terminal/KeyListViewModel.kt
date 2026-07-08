package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File

class KeyListViewModel(app: Application) : AndroidViewModel(app) {
    private val _keys = MutableStateFlow<List<KeyEntry>>(emptyList())
    val keys: StateFlow<List<KeyEntry>> = _keys.asStateFlow()

    private val _pendingDelete = MutableStateFlow<KeyEntry?>(null)
    val pendingDelete: StateFlow<KeyEntry?> = _pendingDelete.asStateFlow()

    private val _generatedPubKey = MutableStateFlow<String?>(null)
    val generatedPubKey: StateFlow<String?> = _generatedPubKey.asStateFlow()

    private val _isGenerating = MutableStateFlow(false)
    val isGenerating: StateFlow<Boolean> = _isGenerating.asStateFlow()

    init { loadKeys() }

    fun loadKeys() {
        viewModelScope.launch(Dispatchers.IO) {
            val list = Repositories.keys.getAll()
            RemoteLogger.i("IsekaiTerminalKey", "loaded ${list.size} key(s): ${list.map { "'${it.label}'" }}")
            _keys.value = list
        }
    }

    fun requestDelete(key: KeyEntry) { _pendingDelete.value = key }
    fun dismissDelete() { _pendingDelete.value = null }

    fun confirmDelete(key: KeyEntry) {
        _pendingDelete.value = null
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("IsekaiTerminalKey", "deleting key id=${key.id} '${key.label}'")
            Repositories.keys.delete(key)
            runCatching { File(key.encryptedPrivateKeyPath).delete() }
            loadKeys()
        }
    }

    fun generateKey(label: String, onError: (String) -> Unit, onSuccess: () -> Unit) {
        if (_isGenerating.value) return
        _isGenerating.value = true
        viewModelScope.launch {
            try {
                val (pemBytes, pubKey) = withContext(Dispatchers.Default) {
                    KeyManager.generateEd25519Pair()
                }
                RemoteLogger.i("IsekaiTerminalKey", "generated ed25519 key pair")
                withContext(Dispatchers.IO) {
                    val app = getApplication<Application>()
                    val path = KeyManager.saveEncryptedKey(app, pemBytes)
                    val id = Repositories.keys.save(
                        KeyEntry(
                            label = label,
                            publicKey = pubKey,
                            encryptedPrivateKeyPath = path,
                            kekAlias = KeyManager.KEK_ALIAS,
                            createdAt = System.currentTimeMillis(),
                        )
                    )
                    RemoteLogger.i("IsekaiTerminalKey", "generated key saved id=$id '$label'")
                }
                _generatedPubKey.value = pubKey
                onSuccess()
                loadKeys()
            } catch (e: Exception) {
                RemoteLogger.e("IsekaiTerminalKey", "key generation failed: ${e.message}", e)
                onError("生成失敗: ${e.message}")
            } finally {
                _isGenerating.value = false
            }
        }
    }

    fun dismissGeneratedPubKey() { _generatedPubKey.value = null }
}
