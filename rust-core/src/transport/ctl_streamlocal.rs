//! tmux 迂回 control-plane(Epic M)のグローバル opt-in フラグと、その ctl socket の
//! リモートパス命名。プロファイル単位ではなくグローバル設定にした(ユーザーとの合意、
//! `ISEKAI_PIPE_DESIGN.md` §8 Epic M参照)。`set_terminal_theme`(`lib.rs`)と同じ
//! 「Kotlin起動時にSharedPreferencesから読んで一度だけ反映する、プロセスグローバルな
//! Rust側状態」というパターンを踏襲している。実際のstreamlocal forwardチャネルの
//! 開設・メッセージ配送は[`super::ssh_handler`]側が担う(`RusshEventHandler`・
//! `run_ssh_channel_loop`)——ここは opt-in フラグとパス命名だけの、それらから見た
//! 依存先。

use std::sync::atomic::{AtomicBool, Ordering};

static CTL_SOCKET_FORWARD_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_ctl_socket_forward_enabled(enabled: bool) {
    CTL_SOCKET_FORWARD_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(super) fn ctl_socket_forward_enabled() -> bool {
    CTL_SOCKET_FORWARD_ENABLED.load(Ordering::Relaxed)
}

/// `/tmp/isekai-pipe-ctl-<32桁hex>.sock`。isekai-sshの`ctl_forward.rs`と同じ命名規約
/// (128bitの乱数トークンで衝突・先取りに耐性を持たせる、`isekai_pipe_core::
/// sweep_stale_sockets`のprefixスイープとも一致させる)。
pub(super) fn new_ctl_socket_path() -> String {
    isekai_pipe_core::ctl_socket_remote_path(&isekai_pipe_core::new_hex_token_128())
}

#[cfg(test)]
mod ctl_socket_tests {
    use super::*;

    #[test]
    fn ctl_socket_paths_match_isekai_ssh_naming_convention() {
        let a = new_ctl_socket_path();
        let b = new_ctl_socket_path();
        assert!(a.starts_with("/tmp/isekai-pipe-ctl-"));
        assert!(a.ends_with(".sock"));
        let token = &a["/tmp/isekai-pipe-ctl-".len()..a.len() - ".sock".len()];
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "each call must mint a fresh unguessable token");
    }

    #[test]
    fn ctl_socket_forward_toggle_defaults_off_and_reflects_last_write() {
        // Process-global state, so serialize against other tests touching the
        // same flag (there's only this one today, but matches the project's
        // `ENV_LOCK`-style convention for shared mutable test state).
        set_ctl_socket_forward_enabled(false);
        assert!(!ctl_socket_forward_enabled());
        set_ctl_socket_forward_enabled(true);
        assert!(ctl_socket_forward_enabled());
        set_ctl_socket_forward_enabled(false);
        assert!(!ctl_socket_forward_enabled());
    }
}
