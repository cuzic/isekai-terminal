package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * SnippetEditViewModel/KeySequenceEditViewModel/ProfileEditViewModelが共通で持つ
 * 「保存中フラグ(isSaving)で多重送信を防ぎつつ、DB書き込みだけをIOディスパッチャへ逃がし、
 * 保存後コールバック(onSaved)は呼び出し元(MainActivity)がnavController.popBackStack()等の
 * UI操作に直結できるようMain(viewModelScopeの既定ディスパッチャ)から呼ぶ」という保存パターンの
 * 基底クラス([DeletableListViewModel]の保存版)。
 *
 * これは単なる重複除去ではなく、ディスパッチャ方針を1箇所に集約して同じクラスのバグ
 * (SnippetEditViewModelが以前`viewModelScope.launch(Dispatchers.IO)`の中で`onSaved()`まで
 * 呼んでおり、IOスレッドからUI操作を行っていた)を再発させないための抽出でもある。
 */
abstract class SavingEditViewModel<T>(app: Application) : AndroidViewModel(app) {
    private val _isSaving = MutableStateFlow(false)
    val isSaving: StateFlow<Boolean> = _isSaving.asStateFlow()

    /** DB保存本体。IOディスパッチャ上で呼ばれる。 */
    protected abstract suspend fun persist(entity: T)

    /** 保存直前(Mainディスパッチャ上、[persist]呼び出しの前)のフック。ログ出力等に使う。 */
    protected open fun onSaving(entity: T) {}

    fun save(entity: T, onSaved: () -> Unit) {
        if (_isSaving.value) return
        _isSaving.value = true
        viewModelScope.launch {
            onSaving(entity)
            withContext(Dispatchers.IO) { persist(entity) }
            _isSaving.value = false
            onSaved()
        }
    }
}
