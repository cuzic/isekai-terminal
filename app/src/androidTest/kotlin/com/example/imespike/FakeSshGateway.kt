package com.example.imespike

import com.example.imespike.session.HostKeyChecker
import com.example.imespike.session.HostKeyDecision
import com.example.imespike.session.SshGateway
import com.example.imespike.session.TsshSession
import uniffi.tssh_core.*

/**
 * テスト用フェイク SshGateway。
 * Rust/ネイティブを一切呼ばず、FakeSshSession を返す。
 */
class FakeSshGateway(val session: FakeSshSession = FakeSshSession()) : SshGateway {
    override fun create(config: SshConfig): TsshSession = session.asTsshSession()
    override fun createQuic(config: QuicConfig): TsshSession = session.asTsshSession()
}

/**
 * テスト用フェイク TsshSession。
 * connect() を呼んでも実際には何もしない。
 * テストコードから simulateXxx() を呼ぶことでコールバックを任意に発火できる。
 */
class FakeSshSession : TsshSession {
    override val isQuic = false
    private var callback: SessionCallback? = null

    val sentBytes = mutableListOf<ByteArray>()
    var lastResizeCols: UInt? = null
    var lastResizeRows: UInt? = null
    var disconnectCalled = false
    var connectCalled = false

    override fun connect(callback: SessionCallback) {
        connectCalled = true
        this.callback = callback
    }

    fun simulateConnected() = callback!!.onConnected()
    fun simulateDisconnected(reason: String? = null) = callback!!.onDisconnected(reason)
    fun simulateData(data: ByteArray) = callback!!.onData(data)
    fun simulateHostKey(fingerprint: String): Boolean = callback!!.onHostKey(fingerprint)
    fun simulateScreenUpdate(update: ScreenUpdate) = callback!!.onScreenUpdate(update)
    fun simulateTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: ULong?) =
        callback!!.onTrzszRequest(transferId, mode, suggestedName, expectedSize)

    fun asTsshSession(): TsshSession = this

    override fun send(data: ByteArray) { sentBytes.add(data) }
    override fun resize(cols: UInt, rows: UInt) { lastResizeCols = cols; lastResizeRows = rows }
    override fun disconnect() { disconnectCalled = true }
    override fun scrollbackLen(): UInt = 0u
    override fun scrollbackCells(offset: UInt, rows: UInt): List<CellData> = emptyList()
    override fun trzszAcceptDownload(transferId: String) {}
    override fun trzszAcceptUpload(transferId: String, fileName: String, fileSize: ULong, mode: UInt) {}
    override fun trzszCancel(transferId: String) {}
    override fun trzszSendChunk(transferId: String, data: ByteArray, isLast: Boolean) {}
}

/** テスト用フェイク HostKeyChecker。デフォルトは常に信頼。 */
class FakeHostKeyChecker(
    private val decision: HostKeyDecision = HostKeyDecision.Trust(isNew = false),
) : HostKeyChecker {
    val checked = mutableListOf<Triple<String, Int, String>>()
    val trusted = mutableListOf<Triple<String, Int, String>>()

    override fun check(host: String, port: Int, fingerprint: String): HostKeyDecision {
        checked.add(Triple(host, port, fingerprint))
        return decision
    }

    override fun trustUpdated(host: String, port: Int, fingerprint: String) {
        trusted.add(Triple(host, port, fingerprint))
    }
}
