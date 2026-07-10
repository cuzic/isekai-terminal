package tools.isekai.terminal

import android.app.Application
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

class KeyListViewModel(app: Application) : DeletableListViewModel<KeyEntry>(app) {
    val keys: StateFlow<List<KeyEntry>> get() = items

    private val _generatedPubKey = MutableStateFlow<String?>(null)
    val generatedPubKey: StateFlow<String?> = _generatedPubKey.asStateFlow()

    private val _isGenerating = MutableStateFlow(false)
    val isGenerating: StateFlow<Boolean> = _isGenerating.asStateFlow()

    init { loadKeys() }

    fun loadKeys() = load()

    override suspend fun fetchAll(): List<KeyEntry> = Repositories.keys.getAll()

    override fun onLoaded(list: List<KeyEntry>) {
        RemoteLogger.i("IsekaiTerminalKey", "loaded ${list.size} key(s): ${list.map { "'${it.label}'" }}")
    }

    override suspend fun deleteItem(item: KeyEntry) {
        RemoteLogger.i("IsekaiTerminalKey", "deleting key id=${item.id} '${item.label}'")
        Repositories.keys.delete(item)
        runCatching { File(item.encryptedPrivateKeyPath).delete() }
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
