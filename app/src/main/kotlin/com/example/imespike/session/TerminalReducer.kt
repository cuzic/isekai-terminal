package com.example.imespike.session

import com.example.imespike.HostKeyChangedWarning
import com.example.imespike.TerminalUiState
import com.example.imespike.TrzszUiState
import uniffi.tssh_core.ScreenUpdate

/**
 * TerminalUiState の純粋な状態遷移関数群。
 * 副作用なし。Android 依存なし（ScreenUpdate は UniFFI data class）。
 *
 * [reduce] が SessionEvent → TerminalUiState の主エントリ。
 * 個別関数はテスト・ViewModel から直接呼ぶこともできる。
 */
object TerminalReducer {

    // ── SessionEvent ディスパッチ ──────────────────────────────────

    fun reduce(s: TerminalUiState, event: SessionEvent): TerminalUiState = when (event) {
        is SessionEvent.Connected         -> connected(s, event.host)
        is SessionEvent.Disconnected      -> disconnected(s, event.reason)
        is SessionEvent.Error             -> error(s, event.message)
        is SessionEvent.ScreenUpdated     -> screenUpdated(s, event.update, event.scrollbackLen)
        is SessionEvent.HostKeyTrusted    -> hostKeyTrusted(s, event.fingerprint)
        is SessionEvent.HostKeyChanged    -> hostKeyChanged(s, event.warning, event.newFingerprint)
        is SessionEvent.TrzszRequest      -> trzszRequest(s, event.transferId, event.mode, event.suggestedName, event.expectedSize)
        is SessionEvent.TrzszProgress     -> trzszProgress(s, event.transferred, event.total)
        is SessionEvent.TrzszFinished     -> trzszFinished(s, event.transferId, event.success, event.message)
        // Data / TrzszDownloadChunk は TerminalSession が副作用処理。状態変更なし。
        is SessionEvent.Data, is SessionEvent.TrzszDownloadChunk -> s
    }

    // ── 個別遷移関数 ─────────────────────────────────────────────

    fun connecting(s: TerminalUiState): TerminalUiState =
        s.copy(statusMsg = "接続中…")

    fun connected(s: TerminalUiState, host: String): TerminalUiState =
        s.copy(connected = true, statusMsg = "接続済み — $host", currentHost = host)

    fun disconnected(s: TerminalUiState, reason: String?): TerminalUiState =
        s.copy(
            connected = false,
            statusMsg = "切断: ${reason ?: "不明"}",
            screenUpdate = null,
            currentHost = null,
        )

    fun error(s: TerminalUiState, message: String): TerminalUiState =
        s.copy(statusMsg = "エラー: $message")

    fun authError(s: TerminalUiState, message: String): TerminalUiState =
        s.copy(statusMsg = message)

    fun screenUpdated(s: TerminalUiState, update: ScreenUpdate, scrollbackLen: Int): TerminalUiState =
        s.copy(screenUpdate = update, scrollbackLen = scrollbackLen)

    fun hostKeyChanged(
        s: TerminalUiState,
        warning: HostKeyChangedWarning,
        newFingerprint: String,
    ): TerminalUiState =
        s.copy(lastFingerprint = newFingerprint, hostKeyChangedWarning = warning)

    fun hostKeyTrusted(s: TerminalUiState, fingerprint: String): TerminalUiState =
        s.copy(lastFingerprint = fingerprint)

    fun trzszRequest(
        s: TerminalUiState,
        transferId: String,
        mode: String,
        suggestedName: String?,
        expectedSize: ULong?,
    ): TerminalUiState =
        s.copy(trzszState = TrzszUiState.WaitingUser(transferId, mode, suggestedName, expectedSize))

    fun trzszProgress(s: TerminalUiState, transferred: ULong, total: ULong?): TerminalUiState =
        when (val cur = s.trzszState) {
            is TrzszUiState.InProgress  -> s.copy(trzszState = cur.copy(transferred = transferred, total = total))
            // 最初の progress イベントで WaitingUser → InProgress に自動遷移
            is TrzszUiState.WaitingUser -> s.copy(
                trzszState = TrzszUiState.InProgress(
                    cur.transferId, cur.mode, cur.suggestedName, transferred, total ?: cur.expectedSize
                )
            )
            else -> s
        }

    fun trzszFinished(
        s: TerminalUiState,
        transferId: String,
        success: Boolean,
        message: String?,
    ): TerminalUiState =
        s.copy(trzszState = TrzszUiState.Done(transferId, success, message))
}
