package com.example.imespike.session

import uniffi.tssh_core.SshConfig
import uniffi.tssh_core.SshSessionInterface
import uniffi.tssh_core.createSshSession

/**
 * SSH セッション生成の境界インターフェース。
 * テストでは FakeSshGateway を注入して Rust/ネイティブ呼出しを回避できる。
 */
interface SshGateway {
    fun create(config: SshConfig): SshSessionInterface
}

/** 本番用実装。UniFFI の createSshSession() を呼ぶ。 */
class DefaultSshGateway : SshGateway {
    override fun create(config: SshConfig): SshSessionInterface = createSshSession(config)
}
