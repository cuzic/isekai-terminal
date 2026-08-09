package tools.isekai.terminal

import android.app.Application
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

class ProfileEditViewModel(app: Application) : SavingEditViewModel<ConnectionProfile>(app) {
    private val _keys = MutableStateFlow<List<KeyEntry>>(emptyList())
    val keys: StateFlow<List<KeyEntry>> = _keys.asStateFlow()

    init {
        viewModelScope.launch(Dispatchers.IO) {
            _keys.value = Repositories.keys.getAll()
        }
    }

    override fun onSaving(entity: ConnectionProfile) {
        RemoteLogger.i("IsekaiTerminalProfile", "saving profile: label='${entity.label}' host=${entity.host}:${entity.port} user=${entity.username} authType=${entity.authType} keyId=${entity.keyId} id=${if (entity.id == 0L) "new" else "${entity.id}"}")
    }

    override suspend fun persist(entity: ConnectionProfile) {
        Repositories.profiles.save(entity) // Room の suspend fun が内部で IO ディスパッチする
    }
}
