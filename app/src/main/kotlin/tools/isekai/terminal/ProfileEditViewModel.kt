package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class ProfileEditViewModel(app: Application) : AndroidViewModel(app) {
    private val _keys = MutableStateFlow<List<KeyEntry>>(emptyList())
    val keys: StateFlow<List<KeyEntry>> = _keys.asStateFlow()

    private val _isSaving = MutableStateFlow(false)
    val isSaving: StateFlow<Boolean> = _isSaving.asStateFlow()

    init {
        viewModelScope.launch(Dispatchers.IO) {
            _keys.value = Repositories.keys.getAll()
        }
    }

    fun save(profile: ConnectionProfile, onSaved: () -> Unit) {
        if (_isSaving.value) return
        _isSaving.value = true
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("TsshProfile", "saving profile: label='${profile.label}' host=${profile.host}:${profile.port} user=${profile.username} authType=${profile.authType} keyId=${profile.keyId} id=${if (profile.id == 0L) "new" else "${profile.id}"}")
            Repositories.profiles.save(profile)
            _isSaving.value = false
            onSaved()
        }
    }
}
