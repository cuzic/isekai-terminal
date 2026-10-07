package tools.isekai.terminal

import android.net.Uri
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.session.HostKeyChecker
import tools.isekai.terminal.session.TerminalSession
import uniffi.isekai_terminal_core.ClipboardPayload
import uniffi.isekai_terminal_core.FilePreviewOutcome
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.OrchestratorCallback
import uniffi.isekai_terminal_core.PlatformFd
import uniffi.isekai_terminal_core.SessionOrchestratorInterface

/*
 * `docs/adr/0020-unwired-callback-detection.md` §3(g)(Phase 2): 本番の[TerminalSession]・[TerminalScreenActions]は
 * 関数型パラメータの既定no-opを持たない(渡し忘れがコンパイルエラーになる)。既定no-opで足りるテストは
 * ここのファクトリ経由で生成する。既定値を持ってよいのはテスト側だけ。
 */

/** 注入lambdaをすべて既定no-opにした[TerminalSession]。検証したいlambdaだけ名前付き引数で渡す。 */
fun testTerminalSession(
    hostKeyChecker: HostKeyChecker = FakeHostKeyChecker(),
    orchestratorFactory: (OrchestratorCallback) -> SessionOrchestratorInterface,
    onClipboardWriteRequested: (ClipboardPayload) -> Unit = {},
    onClipboardPullRequested: () -> ClipboardPayload? = { null },
    acquireWifiFd: () -> PlatformFd? = { null },
    acquireCellularFd: () -> PlatformFd? = { null },
    onBell: () -> Unit = {},
    onNotify: (kind: NotifyKind, title: String, body: String) -> Unit = { _, _, _ -> },
    onNotifyRequested: (NotifyKind) -> Unit = {},
): TerminalSession = TerminalSession(
    hostKeyChecker,
    orchestratorFactory = orchestratorFactory,
    onClipboardWriteRequested = onClipboardWriteRequested,
    onClipboardPullRequested = onClipboardPullRequested,
    acquireWifiFd = acquireWifiFd,
    acquireCellularFd = acquireCellularFd,
    onBell = onBell,
    onNotify = onNotify,
    onNotifyRequested = onNotifyRequested,
)

/** すべての操作をno-opにした[TerminalScreenActions]。検証したい操作だけ名前付き引数で渡す。 */
fun noopTerminalScreenActions(
    onSend: (ByteArray) -> Unit = {},
    onResize: (UInt, UInt) -> Unit = { _, _ -> },
    onTrzszStartUpload: (Uri) -> Unit = {},
): TerminalScreenActions = TerminalScreenActions(
    onConnect = {},
    onDisconnect = {},
    onCancelReconnect = {},
    onBack = {},
    onSend = onSend,
    onResize = onResize,
    onScrollbackCells = { _, _ -> null },
    onSearchScrollback = { _, _ -> emptyList() },
    onJumpToPreviousPrompt = { _, _ -> },
    onJumpToNextPrompt = { _, _ -> },
    onClickToPromptCursor = { _, _ -> },
    onCopyLastCommandOutput = {},
    onTrustUpdatedHostKey = {},
    onDismissHostKeyWarning = {},
    onTrustNewHostKey = {},
    onDismissNewHostKeyPrompt = {},
    onTrzszStartUpload = onTrzszStartUpload,
    onTrzszStartDownload = {},
    onTrzszCancel = {},
    onTrzszDismiss = {},
    onGetSessionLog = { "" },
    onSendSnippet = { _: Snippet -> },
    onSendKeySequence = { _: List<KeyStep> -> },
    onRespondAgentSignRequest = {},
    onRequestFocus = {},
    onNextTab = {},
    onPreviousTab = {},
    onForceReturnToWifi = {},
    onFocusChanged = {},
    onFilePreviewRequest = { FilePreviewOutcome.Error("not connected") },
    onSubmitAiPanelForm = {},
    onDismissAiPanel = {},
)
