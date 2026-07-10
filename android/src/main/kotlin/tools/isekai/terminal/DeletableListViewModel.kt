package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/**
 * ProfileListViewModel/KeyListViewModel/SnippetListViewModelが共通で持つ
 * 「一覧取得→削除確認(requestDelete/dismissDelete)→確定削除(confirmDelete)」
 * パターンの基底クラス。サブクラスは[fetchAll]/[deleteItem]だけ実装すればよい
 * ([onLoaded]は一覧読み込み時のログ出力等、任意のフックとして上書き可能)。
 */
abstract class DeletableListViewModel<T>(app: Application) : AndroidViewModel(app) {
    private val _items = MutableStateFlow<List<T>>(emptyList())
    val items: StateFlow<List<T>> = _items.asStateFlow()

    private val _deleteTarget = MutableStateFlow<T?>(null)
    val deleteTarget: StateFlow<T?> = _deleteTarget.asStateFlow()

    protected abstract suspend fun fetchAll(): List<T>
    protected abstract suspend fun deleteItem(item: T)
    protected open fun onLoaded(list: List<T>) {}

    fun load() {
        viewModelScope.launch(Dispatchers.IO) {
            val list = fetchAll()
            onLoaded(list)
            _items.value = list
        }
    }

    fun requestDelete(item: T) { _deleteTarget.value = item }
    fun dismissDelete() { _deleteTarget.value = null }

    fun confirmDelete(item: T) {
        _deleteTarget.value = null
        viewModelScope.launch(Dispatchers.IO) {
            deleteItem(item)
            load()
        }
    }
}
