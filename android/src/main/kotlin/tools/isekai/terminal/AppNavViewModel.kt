package tools.isekai.terminal

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.data.Snippet

class AppNavViewModel : ViewModel() {
    // 接続対象 (pendingProfile/pendingPassword) は TerminalTabsViewModel.openTab() に
    // 直接渡すようになったため、ここでの中継は不要になった。
    var pendingEditProfile by mutableStateOf<ConnectionProfile?>(null)
    var pendingEditSnippet by mutableStateOf<Snippet?>(null)
    var pendingEditKeySequence by mutableStateOf<KeySequence?>(null)
}
