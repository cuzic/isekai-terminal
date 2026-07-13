package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.data.KeySequencePackInstallation
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.pack.KeySequencePack
import tools.isekai.terminal.pack.KeySequencePacks
import tools.isekai.terminal.util.RemoteLogger

class KeySequenceListViewModel(app: Application) : DeletableListViewModel<KeySequence>(app) {
    val keySequences: StateFlow<List<KeySequence>> get() = items

    // ── 打鍵列セット(パック) ──────────────────────────────
    // MVPではグローバル有効化(profileId=null)のみをこの一覧画面から操作できるようにする
    // (プロファイル別installationの管理はTerminal画面のピッカー以外では未対応)。
    val packs: List<KeySequencePack> = KeySequencePacks.ALL
    private val _globalInstallations = MutableStateFlow<Map<String, KeySequencePackInstallation>>(emptyMap())
    val globalInstallations: StateFlow<Map<String, KeySequencePackInstallation>> = _globalInstallations.asStateFlow()

    init {
        loadKeySequences()
        loadPackInstallations()
    }

    fun loadKeySequences() = load()

    override suspend fun fetchAll(): List<KeySequence> = Repositories.keySequences.getAll()

    override fun onLoaded(list: List<KeySequence>) {
        RemoteLogger.i("IsekaiTerminalKeySequence", "loaded ${list.size} key sequence(s): ${list.map { "'${it.label}'" }}")
    }

    override suspend fun deleteItem(item: KeySequence) {
        RemoteLogger.i("IsekaiTerminalKeySequence", "deleted key sequence id=${item.id} '${item.label}'")
        Repositories.keySequences.delete(item)
    }

    fun loadPackInstallations() {
        viewModelScope.launch(Dispatchers.IO) {
            _globalInstallations.value = packs.mapNotNull { pack ->
                Repositories.keySequencePackInstallations.findGlobal(pack.id)?.let { pack.id to it }
            }.toMap()
        }
    }

    /** [pack]をグローバル有効化(または既存installationのprefixを変更)する。 */
    fun activatePack(pack: KeySequencePack, prefixChar: Char) {
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("IsekaiTerminalKeySequence", "activating pack '${pack.id}' prefix=Ctrl+${prefixChar.uppercaseChar()}")
            Repositories.keySequencePackInstallations.install(
                packId = pack.id,
                version = pack.version,
                paramValues = mapOf("prefix" to KeyStep.CtrlChar(prefixChar)),
            )
            loadPackInstallations()
        }
    }

    fun deactivatePack(installation: KeySequencePackInstallation) {
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("IsekaiTerminalKeySequence", "deactivating pack installation id=${installation.id} packId=${installation.packId}")
            Repositories.keySequencePackInstallations.uninstall(installation)
            loadPackInstallations()
        }
    }
}
