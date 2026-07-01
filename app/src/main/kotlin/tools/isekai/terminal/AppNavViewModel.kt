package tools.isekai.terminal

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import tools.isekai.terminal.data.ConnectionProfile

class AppNavViewModel : ViewModel() {
    var pendingProfile     by mutableStateOf<ConnectionProfile?>(null)
    var pendingPassword    by mutableStateOf<String?>(null)
    var pendingEditProfile by mutableStateOf<ConnectionProfile?>(null)
}
