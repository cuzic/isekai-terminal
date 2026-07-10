package tools.isekai.terminal

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Card
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.ui.DeleteConfirmDialog
import tools.isekai.terminal.util.RemoteLogger

@Composable
fun SnippetListScreen(
    onAddSnippet: () -> Unit,
    onEditSnippet: (Snippet) -> Unit,
    onBack: () -> Unit,
) {
    val vm: SnippetListViewModel = viewModel()
    val snippets by vm.snippets.collectAsStateWithLifecycle()
    val deleteTarget by vm.deleteTarget.collectAsStateWithLifecycle()

    Scaffold(
        topBar = {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .safeDrawingPadding()
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Text(
                    "定型コマンド",
                    fontWeight = FontWeight.Bold,
                    fontSize = 18.sp,
                    modifier = Modifier.align(Alignment.CenterVertically),
                )
                TextButton(onClick = onBack) { Text("戻る") }
            }
        },
        floatingActionButton = {
            FloatingActionButton(onClick = onAddSnippet) {
                Text("＋", fontSize = 24.sp)
            }
        }
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding)
        ) {
            if (snippets.isEmpty()) {
                Text(
                    text = "「＋」をタップして定型コマンドを追加",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.align(Alignment.Center),
                )
            } else {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(8.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    items(snippets, key = { it.id }) { snippet ->
                        SnippetCard(
                            snippet = snippet,
                            onEdit = {
                                RemoteLogger.i("IsekaiTerminalSnippet", "edit: '${snippet.label}' id=${snippet.id}")
                                onEditSnippet(snippet)
                            },
                            onDelete = { vm.requestDelete(snippet) },
                        )
                    }
                }
            }
        }
    }

    deleteTarget?.let { target ->
        DeleteConfirmDialog(
            title = "削除確認",
            message = "「${target.label}」を削除しますか？",
            onConfirm = { vm.confirmDelete(target) },
            onDismiss = { vm.dismissDelete() },
        )
    }
}

@Composable
private fun SnippetCard(
    snippet: Snippet,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onEdit),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = snippet.label,
                    fontWeight = FontWeight.Bold,
                    fontSize = 16.sp,
                )
                Spacer(Modifier.width(2.dp))
                Text(
                    text = snippet.command.lineSequence().firstOrNull() ?: "",
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                )
                Text(
                    text = if (snippet.profileId == null) "全プロファイル共通" else "特定プロファイル専用",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            TextButton(onClick = onEdit) { Text("編集") }
            TextButton(onClick = onDelete) { Text("削除") }
        }
    }
}
