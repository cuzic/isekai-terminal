package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.viewModelScope
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class KeySequenceEditViewModel(app: Application) : SavingEditViewModel<KeySequence>(app) {
    private val _profiles = MutableStateFlow<List<ConnectionProfile>>(emptyList())
    val profiles: StateFlow<List<ConnectionProfile>> = _profiles.asStateFlow()

    init {
        viewModelScope.launch(Dispatchers.IO) {
            _profiles.value = Repositories.profiles.getAll()
        }
    }

    override fun onSaving(entity: KeySequence) {
        RemoteLogger.i(
            "IsekaiTerminalKeySequence",
            "saving key sequence: label='${entity.label}' profileId=${entity.profileId} " +
                "id=${if (entity.id == 0L) "new" else "${entity.id}"}",
        )
    }

    override suspend fun persist(entity: KeySequence) {
        Repositories.keySequences.save(entity)
    }
}
