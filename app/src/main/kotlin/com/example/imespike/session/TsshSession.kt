package com.example.imespike.session

import uniffi.tssh_core.CellData
import uniffi.tssh_core.QuicSessionInterface
import uniffi.tssh_core.SessionCallback
import uniffi.tssh_core.SshSessionInterface

/**
 * TCP SSH (SshSession) と QUIC (QuicSession) の両方を統一するアプリ内インターフェース。
 * UniFFI が生成する各 *Interface は個別型なので、このアダプター層で吸収する。
 */
interface TsshSession {
    fun connect(callback: SessionCallback)
    fun disconnect()
    fun resize(cols: UInt, rows: UInt)
    fun scrollbackCells(offset: UInt, rows: UInt): List<CellData>
    fun scrollbackLen(): UInt
    fun send(data: ByteArray)
    fun trzszAcceptDownload(transferId: String)
    fun trzszAcceptUpload(transferId: String, fileName: String, fileSize: ULong, mode: UInt)
    fun trzszCancel(transferId: String)
    fun trzszSendChunk(transferId: String, data: ByteArray, isLast: Boolean)
}

fun SshSessionInterface.asTsshSession(): TsshSession = object : TsshSession {
    override fun connect(callback: SessionCallback) = this@asTsshSession.connect(callback)
    override fun disconnect() = this@asTsshSession.disconnect()
    override fun resize(cols: UInt, rows: UInt) = this@asTsshSession.resize(cols, rows)
    override fun scrollbackCells(offset: UInt, rows: UInt): List<CellData> = this@asTsshSession.scrollbackCells(offset, rows)
    override fun scrollbackLen(): UInt = this@asTsshSession.scrollbackLen()
    override fun send(data: ByteArray) = this@asTsshSession.send(data)
    override fun trzszAcceptDownload(transferId: String) = this@asTsshSession.trzszAcceptDownload(transferId)
    override fun trzszAcceptUpload(transferId: String, fileName: String, fileSize: ULong, mode: UInt) = this@asTsshSession.trzszAcceptUpload(transferId, fileName, fileSize, mode)
    override fun trzszCancel(transferId: String) = this@asTsshSession.trzszCancel(transferId)
    override fun trzszSendChunk(transferId: String, data: ByteArray, isLast: Boolean) = this@asTsshSession.trzszSendChunk(transferId, data, isLast)
}

fun QuicSessionInterface.asTsshSession(): TsshSession = object : TsshSession {
    override fun connect(callback: SessionCallback) = this@asTsshSession.connect(callback)
    override fun disconnect() = this@asTsshSession.disconnect()
    override fun resize(cols: UInt, rows: UInt) = this@asTsshSession.resize(cols, rows)
    override fun scrollbackCells(offset: UInt, rows: UInt): List<CellData> = this@asTsshSession.scrollbackCells(offset, rows)
    override fun scrollbackLen(): UInt = this@asTsshSession.scrollbackLen()
    override fun send(data: ByteArray) = this@asTsshSession.send(data)
    override fun trzszAcceptDownload(transferId: String) = this@asTsshSession.trzszAcceptDownload(transferId)
    override fun trzszAcceptUpload(transferId: String, fileName: String, fileSize: ULong, mode: UInt) = this@asTsshSession.trzszAcceptUpload(transferId, fileName, fileSize, mode)
    override fun trzszCancel(transferId: String) = this@asTsshSession.trzszCancel(transferId)
    override fun trzszSendChunk(transferId: String, data: ByteArray, isLast: Boolean) = this@asTsshSession.trzszSendChunk(transferId, data, isLast)
}
