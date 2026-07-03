package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class SnippetListViewModel(app: Application) : AndroidViewModel(app) {
    private val _snippets = MutableStateFlow<List<Snippet>>(emptyList())
    val snippets: StateFlow<List<Snippet>> = _snippets.asStateFlow()

    private val _deleteTarget = MutableStateFlow<Snippet?>(null)
    val deleteTarget: StateFlow<Snippet?> = _deleteTarget.asStateFlow()

    init { loadSnippets() }

    fun loadSnippets() {
        viewModelScope.launch(Dispatchers.IO) {
            val list = Repositories.snippets.getAll()
            RemoteLogger.i("TsshSnippet", "loaded ${list.size} snippet(s): ${list.map { "'${it.label}'" }}")
            _snippets.value = list
        }
    }

    fun requestDelete(snippet: Snippet) { _deleteTarget.value = snippet }
    fun dismissDelete() { _deleteTarget.value = null }

    fun confirmDelete(snippet: Snippet) {
        _deleteTarget.value = null
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("TsshSnippet", "deleted snippet id=${snippet.id} '${snippet.label}'")
            Repositories.snippets.delete(snippet)
            loadSnippets()
        }
    }
}
