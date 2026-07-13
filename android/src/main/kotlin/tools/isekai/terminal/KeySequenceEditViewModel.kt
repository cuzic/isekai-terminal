package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
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
import kotlinx.coroutines.withContext

class KeySequenceEditViewModel(app: Application) : AndroidViewModel(app) {
    private val _profiles = MutableStateFlow<List<ConnectionProfile>>(emptyList())
    val profiles: StateFlow<List<ConnectionProfile>> = _profiles.asStateFlow()

    private val _isSaving = MutableStateFlow(false)
    val isSaving: StateFlow<Boolean> = _isSaving.asStateFlow()

    init {
        viewModelScope.launch(Dispatchers.IO) {
            _profiles.value = Repositories.profiles.getAll()
        }
    }

    fun save(keySequence: KeySequence, onSaved: () -> Unit) {
        if (_isSaving.value) return
        _isSaving.value = true
        // onSaved は呼び出し元(MainActivity)でnavController.popBackStack()に直結しており
        // Main threadでの呼び出しを想定するため、DB書き込みだけをIOへ逃がし、onSaved自体は
        // viewModelScopeの既定ディスパッチャ(Main.immediate)へ戻ってから呼ぶ
        // (codexレビュー指摘: SnippetEditViewModelはIOスレッドから直接呼んでいるが、
        // 新規実装としては引き継がない)。
        viewModelScope.launch {
            RemoteLogger.i(
                "IsekaiTerminalKeySequence",
                "saving key sequence: label='${keySequence.label}' profileId=${keySequence.profileId} " +
                    "id=${if (keySequence.id == 0L) "new" else "${keySequence.id}"}",
            )
            withContext(Dispatchers.IO) { Repositories.keySequences.save(keySequence) }
            _isSaving.value = false
            onSaved()
        }
    }
}
