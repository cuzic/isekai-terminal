package tools.isekai.terminal

import android.app.Application
import android.net.Uri
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

class KeyImportViewModel(app: Application) : AndroidViewModel(app) {
    private val _isSaving = MutableStateFlow(false)
    val isSaving: StateFlow<Boolean> = _isSaving.asStateFlow()

    private val _errorMsg = MutableStateFlow<String?>(null)
    val errorMsg: StateFlow<String?> = _errorMsg.asStateFlow()

    fun setError(msg: String) { _errorMsg.value = msg }
    fun clearError() { _errorMsg.value = null }

    fun importKey(uri: Uri, label: String, onSaved: () -> Unit) {
        if (_isSaving.value) return
        _isSaving.value = true
        _errorMsg.value = null
        viewModelScope.launch {
            try {
                val app = getApplication<Application>()
                val pemBytes = withContext(Dispatchers.IO) {
                    app.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                        ?: throw IllegalStateException("ファイルを読み込めませんでした")
                }
                RemoteLogger.i("TsshKey", "read PEM: ${pemBytes.size} bytes")
                withContext(Dispatchers.IO) {
                    val path = KeyManager.saveEncryptedKey(app, pemBytes)
                    val hint = KeyManager.extractPublicKeyHint(pemBytes)
                    RemoteLogger.i("TsshKey", "encrypted key saved → $path")
                    val id = Repositories.keys.save(
                        KeyEntry(
                            label = label,
                            publicKey = hint,
                            encryptedPrivateKeyPath = path,
                            kekAlias = KeyManager.KEK_ALIAS,
                            createdAt = System.currentTimeMillis(),
                        )
                    )
                    RemoteLogger.i("TsshKey", "key saved to DB: id=$id label='$label'")
                }
                onSaved()
            } catch (e: Exception) {
                RemoteLogger.e("TsshKey", "import failed: ${e.message}", e)
                _errorMsg.value = "保存に失敗しました: ${e.message}"
            } finally {
                _isSaving.value = false
            }
        }
    }
}
