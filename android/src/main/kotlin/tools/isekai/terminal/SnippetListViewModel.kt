package tools.isekai.terminal

import android.app.Application
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.flow.StateFlow

class SnippetListViewModel(app: Application) : DeletableListViewModel<Snippet>(app) {
    val snippets: StateFlow<List<Snippet>> get() = items

    init { loadSnippets() }

    fun loadSnippets() = load()

    override suspend fun fetchAll(): List<Snippet> = Repositories.snippets.getAll()

    override fun onLoaded(list: List<Snippet>) {
        RemoteLogger.i("IsekaiTerminalSnippet", "loaded ${list.size} snippet(s): ${list.map { "'${it.label}'" }}")
    }

    override suspend fun deleteItem(item: Snippet) {
        RemoteLogger.i("IsekaiTerminalSnippet", "deleted snippet id=${item.id} '${item.label}'")
        Repositories.snippets.delete(item)
    }
}
