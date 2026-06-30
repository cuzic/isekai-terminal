package com.example.imespike.session

import com.example.imespike.HostKeyChangedWarning
import uniffi.tssh_core.ScreenUpdate

/** UniFFI SessionCallback の各メソッドを密封クラスで表現したイベント。 */
sealed class SessionEvent {
    data class Connected(val host: String) : SessionEvent()
    data class Disconnected(val reason: String?) : SessionEvent()
    data class Data(val bytes: ByteArray) : SessionEvent()
    data class ScreenUpdated(val update: ScreenUpdate, val scrollbackLen: Int) : SessionEvent()
    data class HostKeyTrusted(val fingerprint: String) : SessionEvent()
    data class HostKeyChanged(val warning: HostKeyChangedWarning, val newFingerprint: String) : SessionEvent()
    data class Error(val message: String) : SessionEvent()
    data class TrzszRequest(
        val transferId: String,
        val mode: String,
        val suggestedName: String?,
        val expectedSize: ULong?,
    ) : SessionEvent()
    data class TrzszDownloadChunk(
        val transferId: String,
        val data: ByteArray,
        val isLast: Boolean,
    ) : SessionEvent()
    data class TrzszProgress(
        val transferId: String,
        val transferred: ULong,
        val total: ULong?,
    ) : SessionEvent()
    data class TrzszFinished(
        val transferId: String,
        val success: Boolean,
        val message: String?,
    ) : SessionEvent()
}
