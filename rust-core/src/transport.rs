use std::sync::Arc;

use log::{debug, info, warn};
use russh::{client, ChannelMsg};
use russh_keys::{HashAlg, PrivateKey, PublicKey};

use crate::SshAuth;

// ── Transport command / event ────────────────────────────

/// Kotlin → transport task: SSH I/O 命令
pub(crate) enum TransportCommand {
    WriteStdin(Vec<u8>),
    Resize { cols: u32, rows: u32 },
    Disconnect,
}

/// transport task → session_event_loop: SSH 状態通知
pub(crate) enum TransportEvent {
    HostKey(String, tokio::sync::oneshot::Sender<bool>),
    Connected,
    Stdout(Vec<u8>),
    Resized { cols: u32, rows: u32 },
    Disconnected { reason: Option<String> },
    /// マルチパスtransport専用（`multipath_transport.rs`の`PathBroker`から発火）。
    NoViablePath,
}

/// Kotlin → session_event_loop: trzsz 操作（transport を経由しない）
pub(crate) enum SessionCmd {
    TrzszAcceptUpload  { transfer_id: String, file_name: String, file_size: u64, mode: u32 },
    TrzszChunk         { transfer_id: String, data: Vec<u8>, is_last: bool },
    TrzszAcceptDownload { transfer_id: String },
    TrzszCancel        { transfer_id: String },
}

// ── russh Handler ────────────────────────────────────────

pub(crate) struct RusshEventHandler {
    pub(crate) event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
}

#[async_trait::async_trait]
impl client::Handler for RusshEventHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.event_tx.send(TransportEvent::HostKey(fp, reply_tx)).await.ok();
        Ok(reply_rx.await.unwrap_or(false))
    }
}

// ── SSH チャネルループ（TCP・QUIC 共通）─────────────────

pub(crate) async fn run_ssh_channel_loop(
    username: &str,
    auth: &SshAuth,
    cols: u32,
    rows: u32,
    mut session: client::Handle<RusshEventHandler>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let auth_method = match auth {
        SshAuth::Password { .. } => "password",
        SshAuth::PublicKey { .. } => "pubkey",
    };
    info!("ssh: auth {} for {}", auth_method, username);

    let authenticated = match auth {
        SshAuth::Password { password } => session
            .authenticate_password(username, password)
            .await
            .ok()
            .unwrap_or(false),
        SshAuth::PublicKey { private_key_pem } => {
            match PrivateKey::from_openssh(private_key_pem) {
                Ok(key) => session
                    .authenticate_publickey(username, Arc::new(key))
                    .await
                    .ok()
                    .unwrap_or(false),
                Err(e) => {
                    warn!("ssh: private key parse failed: {}", e);
                    false
                }
            }
        }
    };

    if !authenticated {
        warn!("ssh: auth {} failed for {}", auth_method, username);
        event_tx.send(TransportEvent::Disconnected { reason: Some("Authentication failed".into()) }).await.ok();
        return;
    }
    info!("ssh: auth ok");

    let mut channel = match session.channel_open_session().await {
        Ok(c) => { info!("ssh: session channel opened"); c }
        Err(e) => {
            warn!("ssh: channel_open_session failed: {}", e);
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    info!("ssh: requesting PTY {}x{}", cols, rows);
    if channel.request_pty(false, "xterm-256color", cols, rows, 0, 0, &[]).await.is_err()
        || channel.request_shell(false).await.is_err()
    {
        warn!("ssh: PTY or shell request failed");
        event_tx.send(TransportEvent::Disconnected { reason: Some("PTY/shell request failed".into()) }).await.ok();
        return;
    }
    info!("ssh: shell started — entering I/O loop");

    event_tx.send(TransportEvent::Connected).await.ok();

    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        debug!("ssh: stdout {} bytes", data.len());
                        event_tx.send(TransportEvent::Stdout(data.to_vec())).await.ok();
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        info!("ssh: remote exited status={}", exit_status);
                        event_tx.send(TransportEvent::Disconnected { reason: None }).await.ok();
                        break;
                    }
                    None => {
                        info!("ssh: channel closed by peer");
                        event_tx.send(TransportEvent::Disconnected { reason: None }).await.ok();
                        break;
                    }
                    _ => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(TransportCommand::WriteStdin(data)) => {
                        info!("ssh: stdin {} bytes", data.len());
                        if let Err(e) = channel.data(data.as_ref()).await {
                            warn!("ssh: channel.data write failed: {}", e);
                        }
                    }
                    Some(TransportCommand::Resize { cols, rows }) => {
                        info!("ssh: PTY resize {}x{}", cols, rows);
                        channel.window_change(cols, rows, 0, 0).await.ok();
                        event_tx.send(TransportEvent::Resized { cols, rows }).await.ok();
                    }
                    Some(TransportCommand::Disconnect) | None => {
                        info!("ssh: disconnect requested");
                        channel.eof().await.ok();
                        event_tx.send(TransportEvent::Disconnected { reason: None }).await.ok();
                        break;
                    }
                }
            }
        }
    }
    info!("ssh: I/O loop exited");
}
