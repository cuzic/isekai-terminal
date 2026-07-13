//! SSH transport 層。[`ssh_handler`](russh `client::Handler`実装・接続確立・チャネル
//! I/Oループ本体)・[`forward`](-L/-R/-D ポートフォワードの実体)・
//! [`ctl_streamlocal`](tmux迂回control-plane opt-inフラグ・パス命名)の3モジュールに
//! 分かれている。他クレート内モジュールからは従来通り`crate::transport::X`で
//! アクセスできるよう、外部から参照される型・関数はここで re-export する。

mod ctl_streamlocal;
mod forward;
mod ssh_handler;

pub(crate) use ctl_streamlocal::set_ctl_socket_forward_enabled;
pub(crate) use ssh_handler::{
    authenticate_session, connect_via_jump_or_direct, establish_ssh_handle, establish_ssh_handle_over_stream,
    run_ssh_channel_loop, zeroize_ssh_auth, PooledSshHandle, RusshEventHandler, SessionCmd, TransportCommand,
    TransportEvent,
};
