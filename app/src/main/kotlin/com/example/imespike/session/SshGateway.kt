package com.example.imespike.session

import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshConfig
import uniffi.tssh_core.createQuicSession
import uniffi.tssh_core.createSshSession

/**
 * トランスポート生成の境界インターフェース。
 * テストでは FakeSshGateway を注入して Rust/ネイティブ呼出しを回避できる。
 */
interface SshGateway {
    fun create(config: SshConfig): TsshSession
    fun createQuic(config: QuicConfig): TsshSession
}

/** 本番用実装。UniFFI の createSshSession() / createQuicSession() を呼ぶ。 */
class DefaultSshGateway : SshGateway {
    override fun create(config: SshConfig): TsshSession =
        createSshSession(config).asTsshSession()

    override fun createQuic(config: QuicConfig): TsshSession =
        createQuicSession(config).asTsshSession()
}
