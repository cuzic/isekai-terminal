package tools.isekai.terminal

import android.app.Application
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

class ProfileListViewModel(app: Application) : DeletableListViewModel<ConnectionProfile>(app) {
    val profiles: StateFlow<List<ConnectionProfile>> get() = items

    private val _passwordTarget = MutableStateFlow<ConnectionProfile?>(null)
    val passwordTarget: StateFlow<ConnectionProfile?> = _passwordTarget.asStateFlow()

    init { loadProfiles() }

    fun loadProfiles() = load()

    override suspend fun fetchAll(): List<ConnectionProfile> = Repositories.profiles.getAll()

    override fun onLoaded(list: List<ConnectionProfile>) {
        RemoteLogger.i("IsekaiTerminalProfile", "loaded ${list.size} profile(s): ${list.map { "'${it.label}'" }}")
    }

    override suspend fun deleteItem(item: ConnectionProfile) {
        RemoteLogger.i("IsekaiTerminalProfile", "deleted profile id=${item.id} '${item.label}'")
        Repositories.profiles.delete(item)
    }

    fun requestPasswordConnect(profile: ConnectionProfile) { _passwordTarget.value = profile }
    fun dismissPassword() { _passwordTarget.value = null }
}
