package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class SnippetEditViewModel(app: Application) : AndroidViewModel(app) {
    private val _profiles = MutableStateFlow<List<ConnectionProfile>>(emptyList())
    val profiles: StateFlow<List<ConnectionProfile>> = _profiles.asStateFlow()

    private val _isSaving = MutableStateFlow(false)
    val isSaving: StateFlow<Boolean> = _isSaving.asStateFlow()

    init {
        viewModelScope.launch(Dispatchers.IO) {
            _profiles.value = Repositories.profiles.getAll()
        }
    }

    fun save(snippet: Snippet, onSaved: () -> Unit) {
        if (_isSaving.value) return
        _isSaving.value = true
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("IsekaiTerminalSnippet", "saving snippet: label='${snippet.label}' profileId=${snippet.profileId} id=${if (snippet.id == 0L) "new" else "${snippet.id}"}")
            Repositories.snippets.save(snippet)
            _isSaving.value = false
            onSaved()
        }
    }
}
