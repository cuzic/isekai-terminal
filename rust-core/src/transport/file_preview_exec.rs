//! タスク#17(ファイルプレビュー機能): `TransportCommand::FilePreviewExec`の実体。
//! `super::ssh_handler::run_ssh_channel_loop`の対話シェルPTYチャネルとは別に、同じ
//! `client::Handle`上へもう1本`exec`チャネルを開いて1回限りのコマンドを実行し、
//! 標準出力を丸ごと集めてから`TransportEvent::FilePreviewExecResult`で返す。
//!
//! `helper_bootstrap.rs`の`run_exec`(isekai-pipe自動配布用の`&mut client::Handle`版)と
//! 同じ`channel_open_session → exec → data(stdin)? → eof → wait loop`の骨格だが、
//! こちらは`Arc<tokio::sync::Mutex<client::Handle<..>>>`を受け取り、ロックは
//! `channel_open_session`呼び出しの間だけ保持する(`super::forward::run_local_forward`と
//! 同じ「対話シェルのI/Oループを止めないよう、Handleのロックは必要な操作の間だけ」
//! という規約)。

use std::sync::Arc;

use log::{debug, warn};
use russh::ChannelMsg;

use super::ssh_handler::{RusshEventHandler, TransportEvent};

pub(crate) async fn run_file_preview_exec(
    request_id: String,
    command_line: String,
    session: Arc<tokio::sync::Mutex<russh::client::Handle<RusshEventHandler>>>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let mut channel = match session.lock().await.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            warn!("file-preview[{}]: channel_open_session failed: {}", request_id, e);
            // execチャネル自体を開けなかった(接続断等)。stdoutは空・exit_statusはNoneで
            // 返す — `crate::file_preview::parse_result`が`exit_status != Some(0)`を
            // 汎用エラーとして扱う。
            event_tx.send(TransportEvent::FilePreviewExecResult { request_id, stdout: Vec::new(), exit_status: None })
                .await.ok();
            return;
        }
    };

    if let Err(e) = channel.exec(true, command_line.as_str()).await {
        warn!("file-preview[{}]: exec failed: {}", request_id, e);
        event_tx.send(TransportEvent::FilePreviewExecResult { request_id, stdout: Vec::new(), exit_status: None })
            .await.ok();
        return;
    }

    let mut stdout = Vec::new();
    let mut exit_status = None;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                if let Ok(s) = std::str::from_utf8(&data) {
                    debug!("file-preview[{}]: stderr: {}", request_id, s);
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                exit_status = Some(status);
            }
            None => break,
            _ => {}
        }
    }

    debug!("file-preview[{}]: done, {} bytes, exit_status={:?}", request_id, stdout.len(), exit_status);
    event_tx.send(TransportEvent::FilePreviewExecResult { request_id, stdout, exit_status }).await.ok();
}
