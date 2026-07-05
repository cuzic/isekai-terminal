package tools.isekai.terminal

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class ProfileListViewModel(app: Application) : AndroidViewModel(app) {
    private val _profiles = MutableStateFlow<List<ConnectionProfile>>(emptyList())
    val profiles: StateFlow<List<ConnectionProfile>> = _profiles.asStateFlow()

    private val _passwordTarget = MutableStateFlow<ConnectionProfile?>(null)
    val passwordTarget: StateFlow<ConnectionProfile?> = _passwordTarget.asStateFlow()

    private val _deleteTarget = MutableStateFlow<ConnectionProfile?>(null)
    val deleteTarget: StateFlow<ConnectionProfile?> = _deleteTarget.asStateFlow()

    init { loadProfiles() }

    fun loadProfiles() {
        viewModelScope.launch(Dispatchers.IO) {
            val list = Repositories.profiles.getAll()
            RemoteLogger.i("IsekaiTerminalProfile", "loaded ${list.size} profile(s): ${list.map { "'${it.label}'" }}")
            _profiles.value = list
        }
    }

    fun requestPasswordConnect(profile: ConnectionProfile) { _passwordTarget.value = profile }
    fun dismissPassword() { _passwordTarget.value = null }
    fun requestDelete(profile: ConnectionProfile) { _deleteTarget.value = profile }
    fun dismissDelete() { _deleteTarget.value = null }

    fun confirmDelete(profile: ConnectionProfile) {
        _deleteTarget.value = null
        viewModelScope.launch(Dispatchers.IO) {
            RemoteLogger.i("IsekaiTerminalProfile", "deleted profile id=${profile.id} '${profile.label}'")
            Repositories.profiles.delete(profile)
            loadProfiles()
        }
    }
}
