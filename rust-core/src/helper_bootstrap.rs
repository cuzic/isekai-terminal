//! Phase 7-3: SSH 経由で isekai-helper をリモートへ自動配布・起動するブートストラップロジック。
//!
//! 契約の詳細は `/HELPER_PROTOCOL.md` を参照。ここでは既に認証済みの `russh::client::Handle`
//! 上で、以下を行う:
//!
//! 1. 既存インストール確認（`~/.local/bin/isekai-helper` の存在・バージョン確認）
//! 2. 無ければ（またはバージョン不一致なら）対応アーキテクチャのバイナリを転送
//! 3. 起動し、標準出力からハンドシェイク JSON を受け取る
//!
//! 転送は SFTP ではなく `base64 -d > file` 形式の exec で行う（sshd 側の SFTP subsystem
//! 設定に依存しないため、より広い環境で動く）。

use std::time::Duration;

use base64::Engine as _;
use log::{info, warn};
use russh::{client, ChannelMsg};
use serde::Deserialize;

use crate::transport::RusshEventHandler;

const HELPER_INSTALL_DIR: &str = "~/.local/bin";
const HELPER_BIN_NAME: &str = "isekai-helper";
const HANDSHAKE_DIR: &str = "~/.cache/isekai-terminal";
const HANDSHAKE_FILE: &str = "~/.cache/isekai-terminal/helper.handshake";
const HANDSHAKE_LOG: &str = "~/.cache/isekai-terminal/helper.log";
/// 起動後、ハンドシェイク行が書き出されるまでのポーリング回数・間隔。
const HANDSHAKE_POLL_ATTEMPTS: u32 = 50;
const HANDSHAKE_POLL_INTERVAL_MS: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("ssh exec failed: {0}")]
    Exec(String),
    #[error("unsupported remote architecture: {0}")]
    UnsupportedArch(String),
    #[error("failed to upload isekai-helper binary: {0}")]
    Upload(String),
    #[error("failed to launch isekai-helper: {0}")]
    Launch(String),
    #[error("handshake not received within timeout")]
    HandshakeTimeout,
    #[error("failed to parse handshake JSON: {0}")]
    HandshakeParse(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelperHandshake {
    pub v: u32,
    pub listen_port: u16,
    pub cert_sha256: String,
    pub session_secret: String,
}

/// 既知の Linux アーキテクチャ用にビルドした isekai-helper の静的バイナリ。
/// 呼び出し側（Kotlin/Android ビルド成果物、または include_bytes!）が供給する。
pub struct HelperBinaries<'a> {
    pub x86_64: &'a [u8],
    pub aarch64: &'a [u8],
}

impl<'a> HelperBinaries<'a> {
    fn select_for(&self, uname_m: &str) -> Result<&'a [u8], BootstrapError> {
        match uname_m.trim() {
            "x86_64" => Ok(self.x86_64),
            "aarch64" | "arm64" => Ok(self.aarch64),
            other => Err(BootstrapError::UnsupportedArch(other.to_string())),
        }
    }
}

/// 1本の exec チャネルでコマンドを実行し、（任意で）stdin にバイト列を書き込み、
/// stdout を収集して (stdout, exit_status) を返す。
async fn run_exec(
    session: &mut client::Handle<RusshEventHandler>,
    command: &str,
    stdin: Option<&[u8]>,
) -> Result<(Vec<u8>, Option<u32>), BootstrapError> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| BootstrapError::Exec(format!("channel_open_session: {e}")))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| BootstrapError::Exec(format!("exec({command:?}): {e}")))?;

    if let Some(data) = stdin {
        channel
            .data(data)
            .await
            .map_err(|e| BootstrapError::Exec(format!("stdin write: {e}")))?;
    }
    channel
        .eof()
        .await
        .map_err(|e| BootstrapError::Exec(format!("eof: {e}")))?;

    let mut stdout = Vec::new();
    let mut exit_status = None;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                // stderr はログ用途のみ。デバッグレベルで残す。
                if let Ok(s) = std::str::from_utf8(&data) {
                    log::debug!("ssh exec stderr: {s}");
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                exit_status = Some(status);
            }
            None => break,
            _ => {}
        }
    }
    Ok((stdout, exit_status))
}

/// isekai-helper が既にインストール済みかつ起動可能なバージョンかを確認する。
async fn check_existing_version(
    session: &mut client::Handle<RusshEventHandler>,
    expected_version: &str,
) -> bool {
    let cmd = format!(
        "test -x {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} && {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} --version"
    );
    match run_exec(session, &cmd, None).await {
        Ok((stdout, Some(0))) => {
            let out = String::from_utf8_lossy(&stdout);
            out.contains(expected_version)
        }
        _ => false,
    }
}

/// バイナリを `~/.local/bin/isekai-helper` へ転送する（base64 + exec、SFTP subsystem 不要）。
/// ファイル権限は 0700（HELPER_PROTOCOL.md のブートストラップ側契約）。
async fn upload_binary(
    session: &mut client::Handle<RusshEventHandler>,
    binary: &[u8],
) -> Result<(), BootstrapError> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
    let cmd = format!(
        "umask 077 && mkdir -p {HELPER_INSTALL_DIR} && \
         base64 -d > {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp && \
         chmod 0700 {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp && \
         mv {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"
    );
    let (stdout, exit_status) = run_exec(session, &cmd, Some(encoded.as_bytes())).await?;
    if exit_status != Some(0) {
        return Err(BootstrapError::Upload(format!(
            "upload command exited with {:?}: {}",
            exit_status,
            String::from_utf8_lossy(&stdout)
        )));
    }
    Ok(())
}

/// isekai-helper を起動し、標準出力のハンドシェイク行をポーリングで取得する。
/// `--max-idle-lifetime`/`--idle-timeout` は既定値に任せる（呼び出し側で変えたい場合は
/// target_addr 以外の追加引数を別途サポートする拡張余地を残す）。
async fn launch_and_capture_handshake(
    session: &mut client::Handle<RusshEventHandler>,
    ssh_relay_target: &str,
    bind_port: Option<u16>,
) -> Result<HelperHandshake, BootstrapError> {
    // ファイル権限は 0700(dir)/0600(handshake ファイル) を umask で保証する
    // （HELPER_PROTOCOL.md「Bootstrap file permissions」契約）。
    //
    // 実機検証の結果、`cmd & disown`（引数無しはもちろん `disown -a` でも）では、
    // 起動元シェル（russh の exec チャネルが実行する `bash -c "..."` 本体）が
    // スクリプル終了後も長時間 `do_wait()` に留まり、isekai-helper（長時間稼働する
    // デーモン）の終了を待ち続けてしまうことを確認した。`( ... & )` のようにサブシェルで
    // 一段包んでバックグラウンド化すると、外側シェルの直接の子はサブシェル（即座に終了）
    // だけになり、isekai-helper は孫プロセスとして完全に独立するため、この問題が起きない。
    //
    // `bind_port`: Tailscale経由（path0のみ）は`ts-input`チェーンが素通しなので既定の
    // エフェメラルポート（`0.0.0.0:0`）のままでよいが、direct_host（外部到達アドレス、
    // Wi-Fi/セルラー物理pathも同じ宛先ポートを使う）はホストのiptables許可リストに
    // 載った固定ポートでないと外形疎通できない（実機検証で確認、Phase 9-4）。
    // `[::]:port`（IPv6ワイルドカード）でbindすると、noqの`Endpoint::server()`が
    // 内部で`set_only_v6(false)`する（IPv6アドレスでbindした場合のみの挙動）ため、
    // 同一ソケットがIPv4/IPv6両方のパケットを受け付けるdual-stackになる（実機で
    // 確認済み、Phase 9-4追加調査）。`0.0.0.0`（IPv4のみ）ではこの恩恵が無い。
    let bind_arg = match bind_port {
        Some(port) => format!("--bind [::]:{port} "),
        None => String::new(),
    };
    let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;
    let launch_cmd = format!(
        "umask 077 && mkdir -p {HANDSHAKE_DIR} && \
         ( setsid {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} {bind_arg}--target {ssh_relay_target} \
         </dev/null >{HANDSHAKE_FILE} 2>{HANDSHAKE_LOG} & ); \
         for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
           [ -s {HANDSHAKE_FILE} ] && break; \
           sleep {sleep_secs}; \
         done; \
         cat {HANDSHAKE_FILE}"
    );

    let (stdout, _exit_status) = run_exec(session, &launch_cmd, None).await?;
    let first_line = stdout
        .split(|&b| b == b'\n')
        .find(|line| !line.is_empty())
        .ok_or(BootstrapError::HandshakeTimeout)?;

    serde_json::from_slice(first_line)
        .map_err(|e| BootstrapError::HandshakeParse(e.to_string()))
}

/// isekai-helper が起動していることを保証し、ハンドシェイクを返す。
/// 既存インストールが使えればそれを再利用し、無ければ配布・起動する。
pub async fn ensure_helper_running(
    session: &mut client::Handle<RusshEventHandler>,
    binaries: &HelperBinaries<'_>,
    expected_version: &str,
    ssh_relay_target: &str,
    bind_port: Option<u16>,
) -> Result<HelperHandshake, BootstrapError> {
    if !check_existing_version(session, expected_version).await {
        info!("isekai-helper: no matching existing install, detecting remote arch");
        let (uname_out, _) = run_exec(session, "uname -m", None).await?;
        let uname_m = String::from_utf8_lossy(&uname_out);
        let binary = binaries.select_for(&uname_m)?;
        info!("isekai-helper: uploading binary for {}", uname_m.trim());
        upload_binary(session, binary).await?;
    } else {
        info!("isekai-helper: existing install matches expected version, reusing");
    }

    match tokio::time::timeout(
        Duration::from_secs(10),
        launch_and_capture_handshake(session, ssh_relay_target, bind_port),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            warn!("isekai-helper: launch/handshake timed out");
            Err(BootstrapError::HandshakeTimeout)
        }
    }
}

#[cfg(test)]
mod tests {
    //! ローカルの実 sshd（127.0.0.1:22）に対する E2E テスト。
    //! 実 sshd（127.0.0.1:22）+ 事前に authorized_keys へ登録したテスト用鍵が必要な
    //! 実機 E2E テスト。`HELPER_BOOTSTRAP_TEST_KEY`（鍵ファイルパス）が設定されていない
    //! 環境では自動的にスキップする（CI/他の開発者の `cargo test` を壊さないようにするため。
    //! opt-in 方式であり、明示的な SKIP フラグの有無に依存しない）。
    use super::*;
    use std::sync::Arc;

    fn test_key_path() -> Option<String> {
        std::env::var("HELPER_BOOTSTRAP_TEST_KEY").ok()
    }

    async fn connect_test_session(key_path: &str) -> client::Handle<RusshEventHandler> {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        // host key は常に承認する（テスト専用）。
        tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                if let crate::transport::TransportEvent::HostKey(_, reply) = ev {
                    let _ = reply.send(true);
                }
            }
        });

        let config = Arc::new(client::Config::default());
        let handler = RusshEventHandler::new(event_tx);
        let mut session = client::connect(config, ("127.0.0.1", 22), handler)
            .await
            .expect("failed to connect to local sshd on 127.0.0.1:22");

        let key_pem = std::fs::read_to_string(key_path).unwrap();
        let key = russh_keys::PrivateKey::from_openssh(&key_pem).unwrap();
        let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
        let ok = session
            .authenticate_publickey(user, Arc::new(key))
            .await
            .expect("auth request failed");
        assert!(ok, "publickey auth to local sshd failed");
        session
    }

    fn read_musl_binary(target: &str) -> Vec<u8> {
        let path = format!(
            "{}/target/{}/release/isekai-helper",
            env!("CARGO_MANIFEST_DIR"),
            target
        );
        std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read {path} (build it first via rust-core/scripts/build-isekai-helper-musl.sh): {e}"))
    }

    #[tokio::test]
    async fn bootstraps_and_launches_helper_over_real_ssh() {
        let Some(key_path) = test_key_path() else {
            eprintln!(
                "skipping: HELPER_BOOTSTRAP_TEST_KEY not set (requires a real sshd + registered test key)"
            );
            return;
        };
        let mut session = connect_test_session(&key_path).await;

        // クリーンな状態から検証するため、既存インストールを削除しておく。
        let _ = run_exec(
            &mut session,
            &format!("rm -f {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"),
            None,
        )
        .await;

        let x86_64_bin = read_musl_binary("x86_64-unknown-linux-musl");
        let aarch64_bin = read_musl_binary("aarch64-unknown-linux-musl");
        let binaries = HelperBinaries {
            x86_64: &x86_64_bin,
            aarch64: &aarch64_bin,
        };

        let handshake = ensure_helper_running(&mut session, &binaries, "0.1.0", "127.0.0.1:22", None)
            .await
            .expect("bootstrap failed");
        assert_eq!(handshake.v, 1);
        assert!(handshake.listen_port > 0);
        assert_eq!(handshake.cert_sha256.len(), 64);

        // 2回目呼び出し: バイナリは既にインストール済みのはずなので、
        // アップロードをスキップしても正常にハンドシェイクを取得できることを確認する。
        let handshake2 = ensure_helper_running(&mut session, &binaries, "0.1.0", "127.0.0.1:22", None)
            .await
            .expect("second bootstrap call failed");
        assert_eq!(handshake2.v, 1);
        // 起動のたびに ephemeral cert/secret を生成するため、値自体は毎回変わる。
        assert_ne!(handshake.session_secret, handshake2.session_secret);
    }
}
