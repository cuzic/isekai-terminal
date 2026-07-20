package tools.isekai.terminal.filepreview

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ArrowBack
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Description
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import tools.isekai.terminal.formatBytes
import tools.isekai.terminal.ui.AppColors
import uniffi.isekai_terminal_core.FilePreviewEntry
import uniffi.isekai_terminal_core.FilePreviewOutcome
import uniffi.isekai_terminal_core.FilePreviewRequestKind

/**
 * タスク#17(ファイルプレビュー機能): リモートのディレクトリブラウザ + ファイルビューア
 * (Markdown/画像/CSV/シンタックスハイライト付きテキスト)を1つのシートにまとめたもの。
 *
 * 状態(現在ブラウズ中のパス・開いているビューアの種類)はUI表示専用のナビゲーション
 * 状態としてこのComposable内の`remember`に閉じており(`.claude/rules/rust-ssot.md`の
 * 「UI表示だけに閉じた状態」の例外 — `isekai-pipe ctl file`は`ls`/`cat`ごとに独立した
 * ステートレスな呼び出しであり、サーバー側にもカーソル等のセッション状態は無い)、
 * [onRequest]経由でRust側(`SessionOrchestrator::filePreviewRequest`)へ都度問い合わせる。
 *
 * trzsz転送シート(`TrzszTransferSheet`)との導線: このシートはあくまで「中身を覗く」
 * 読み取り専用プレビューであり、実際のダウンロード/アップロードはtrzsz(`# rz`/`# sz`を
 * ターミナルで実行)に任せる。[onOpenTerminalForTransfer]はビューア画面のツールバーから
 * 「ターミナルでtrzsz転送」を選んだ際にシート自体を閉じてターミナル入力へフォーカスを
 * 戻すためのフック(実際のtrzsz起動はユーザーがシェル上でコマンドを打つ、既存の
 * trzsz導線どおり)。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun FilePreviewSheet(
    onRequest: suspend (FilePreviewRequestKind) -> FilePreviewOutcome,
    onDismiss: () -> Unit,
    initialPath: String = "~",
    onOpenTerminalForTransfer: () -> Unit = {},
) {
    var browserState by remember { mutableStateOf(FileBrowserUiState(currentPath = initialPath, isLoading = true)) }
    var viewerState by remember { mutableStateOf<FileViewerUiState?>(null) }
    val scope = rememberCoroutineScope()

    fun navigateTo(path: String) {
        viewerState = null
        browserState = FileBrowserUiState(currentPath = path, isLoading = true)
        scope.launch {
            when (val outcome = onRequest(FilePreviewRequestKind.Ls(path))) {
                is FilePreviewOutcome.Ls -> {
                    val sorted = outcome.entries.sortedWith(
                        compareByDescending<FilePreviewEntry> { it.isDir }.thenBy { it.name.lowercase() },
                    )
                    browserState = FileBrowserUiState(currentPath = path, entries = sorted)
                }
                is FilePreviewOutcome.Error ->
                    browserState = FileBrowserUiState(currentPath = path, errorMessage = outcome.message)
                else ->
                    browserState = FileBrowserUiState(currentPath = path, errorMessage = "unexpected response")
            }
        }
    }

    fun openFile(entry: FilePreviewEntry) {
        val path = FilePreviewPaths.join(browserState.currentPath, entry.name)
        val kind = FilePreviewKindDetector.detect(entry.name)
        viewerState = FileViewerUiState(path = path, kind = kind, isLoading = true, totalSize = entry.size.toLong())
        scope.launch {
            val maxBytes = if (kind == FilePreviewKind.IMAGE) {
                FilePreviewLoader.MAX_IMAGE_PREVIEW_BYTES
            } else {
                FilePreviewLoader.MAX_TEXT_PREVIEW_BYTES
            }
            try {
                val (bytes, totalSize, truncated) = FilePreviewLoader.loadBytes(path, maxBytes, onRequest) { loaded, total ->
                    viewerState = viewerState?.copy(loadedBytes = loaded, totalSize = total)
                }
                viewerState = if (kind == FilePreviewKind.IMAGE) {
                    viewerState?.copy(isLoading = false, imageBytes = bytes, totalSize = totalSize, truncated = truncated)
                } else {
                    viewerState?.copy(
                        isLoading = false,
                        textContent = bytes.toString(Charsets.UTF_8),
                        totalSize = totalSize,
                        truncated = truncated,
                    )
                }
            } catch (e: FilePreviewLoadError) {
                viewerState = viewerState?.copy(isLoading = false, errorMessage = e.message)
            }
        }
    }

    LaunchedEffect(Unit) { navigateTo(initialPath) }

    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(modifier = Modifier.fillMaxSize().height(560.dp)) {
            val currentViewer = viewerState
            if (currentViewer != null) {
                FileViewerHeader(
                    state = currentViewer,
                    onBack = { viewerState = null },
                    onClose = onDismiss,
                    onOpenTerminalForTransfer = onOpenTerminalForTransfer,
                )
                FileViewerBody(currentViewer, modifier = Modifier.fillMaxSize())
            } else {
                FileBrowserHeader(
                    state = browserState,
                    onNavigateUp = { navigateTo(FilePreviewPaths.parent(browserState.currentPath)) },
                    onClose = onDismiss,
                )
                FileBrowserBody(
                    state = browserState,
                    onEntryClick = { entry ->
                        if (entry.isDir) navigateTo(FilePreviewPaths.join(browserState.currentPath, entry.name))
                        else openFile(entry)
                    },
                    modifier = Modifier.fillMaxSize(),
                )
            }
        }
    }
}

@Composable
private fun FileBrowserHeader(state: FileBrowserUiState, onNavigateUp: () -> Unit, onClose: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        IconButton(onClick = onNavigateUp) { Icon(Icons.Default.ArrowBack, contentDescription = "上の階層へ") }
        Text(
            state.currentPath,
            modifier = Modifier.weight(1f).padding(horizontal = 4.dp),
            color = AppColors.MutedText,
            fontSize = 14.sp,
            maxLines = 1,
        )
        IconButton(onClick = onClose) { Icon(Icons.Default.Close, contentDescription = "閉じる") }
    }
}

@Composable
private fun FileBrowserBody(state: FileBrowserUiState, onEntryClick: (FilePreviewEntry) -> Unit, modifier: Modifier = Modifier) {
    when {
        state.isLoading -> Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
        state.errorMessage != null -> Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Text(state.errorMessage, color = AppColors.Error, modifier = Modifier.padding(24.dp))
        }
        state.entries.isEmpty() -> Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Text("空のディレクトリです", color = AppColors.SecondaryText)
        }
        else -> LazyColumn(modifier = modifier) {
            items(state.entries) { entry ->
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .clickable { onEntryClick(entry) }
                        .padding(horizontal = 16.dp, vertical = 10.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Icon(
                        if (entry.isDir) Icons.Default.Folder else Icons.Default.Description,
                        contentDescription = null,
                        tint = if (entry.isDir) AppColors.Warning else AppColors.SecondaryText,
                    )
                    Text(entry.name, modifier = Modifier.weight(1f), color = AppColors.MutedText, fontSize = 14.sp)
                    if (!entry.isDir) {
                        Text(formatBytes(entry.size.toLong()), color = AppColors.SecondaryText, fontSize = 11.sp)
                    }
                }
            }
        }
    }
}

@Composable
private fun FileViewerHeader(
    state: FileViewerUiState,
    onBack: () -> Unit,
    onClose: () -> Unit,
    onOpenTerminalForTransfer: () -> Unit,
) {
    Column {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) { Icon(Icons.Default.ArrowBack, contentDescription = "一覧に戻る") }
            Text(
                state.path.substringAfterLast('/'),
                modifier = Modifier.weight(1f).padding(horizontal = 4.dp),
                color = AppColors.MutedText,
                fontSize = 14.sp,
                maxLines = 1,
            )
            IconButton(onClick = onClose) { Icon(Icons.Default.Close, contentDescription = "閉じる") }
        }
        Row(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = Arrangement.SpaceBetween) {
            if (state.truncated) {
                Text("表示上限に達したため一部のみ表示しています", color = AppColors.Warning, fontSize = 11.sp)
            } else {
                Text("", fontSize = 11.sp)
            }
            Text(
                "大きなファイル全体を取得するにはターミナルでtrzsz(sz/rz)を使ってください",
                color = AppColors.SecondaryText,
                fontSize = 11.sp,
                modifier = Modifier.clickable(onClick = onOpenTerminalForTransfer),
            )
        }
    }
}

@Composable
private fun FileViewerBody(state: FileViewerUiState, modifier: Modifier = Modifier) {
    when {
        state.isLoading -> Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator()
                if (state.totalSize > 0) {
                    Text(
                        "${formatBytes(state.loadedBytes)} / ${formatBytes(state.totalSize)}",
                        color = AppColors.SecondaryText,
                        fontSize = 12.sp,
                        modifier = Modifier.padding(top = 8.dp),
                    )
                }
            }
        }
        state.errorMessage != null -> Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Text(state.errorMessage, color = AppColors.Error, modifier = Modifier.padding(24.dp))
        }
        state.kind == FilePreviewKind.IMAGE && state.imageBytes != null ->
            ImageViewer(state.imageBytes, modifier = modifier)
        state.kind == FilePreviewKind.MARKDOWN && state.textContent != null ->
            MarkdownViewer(state.textContent, modifier = modifier.padding(12.dp))
        state.kind == FilePreviewKind.CSV && state.textContent != null ->
            CsvViewer(state.path.substringAfterLast('/'), state.textContent, modifier = modifier)
        state.kind == FilePreviewKind.TEXT && state.textContent != null ->
            SyntaxHighlightedTextViewer(state.path.substringAfterLast('/'), state.textContent, modifier = modifier)
        else -> Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Text("表示できるコンテンツがありません", color = AppColors.SecondaryText)
        }
    }
}
