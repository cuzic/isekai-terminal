//! tmux 迂回 control-plane(Epic M)のグローバル opt-in フラグと、その ctl socket の
//! リモートパス命名。プロファイル単位ではなくグローバル設定にした(ユーザーとの合意、
//! `ISEKAI_PIPE_DESIGN.md` §8 Epic M参照)。`set_terminal_theme`(`lib.rs`)と同じ
//! 「Kotlin起動時にSharedPreferencesから読んで一度だけ反映する、プロセスグローバルな
//! Rust側状態」というパターンを踏襲している。実際のstreamlocal forwardチャネルの
//! 開設・メッセージ配送は[`super::ssh_handler`]側が担う(`RusshEventHandler`・
//! `run_ssh_channel_loop`)——ここは opt-in フラグとパス命名だけの、それらから見た
//! 依存先。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use isekai_protocol::{CtlVarStore, VarScope};

static CTL_SOCKET_FORWARD_ENABLED: AtomicBool = AtomicBool::new(false);

/// The `setvar`/`getvar` store backing `VarScope::Global` (task #16): unlike
/// `VarScope::Tab`/`VarScope::Session` (a store per tab, owned by
/// `ssh_handler::run_ssh_channel_loop`), `Global` is meant to span every tab
/// in this one Android app process, so it's a single process-wide instance
/// here — this crate's one-process-many-tabs shape is exactly what makes
/// `Global` meaningfully different from `Tab`/`Session` here, unlike
/// `isekai-ssh`'s one-process-per-tab CLI wrapper (see that crate's
/// `ctl_forward.rs` for the same trade-off spelled out there).
static GLOBAL_CTL_VARS: LazyLock<CtlVarStore> = LazyLock::new(CtlVarStore::new);

/// Resolves which `CtlVarStore` a `setvar`/`getvar` should read/write:
/// `tab_store` (this tab's own store) for `Tab`/`Session`, or the
/// process-wide `GLOBAL_CTL_VARS` for `Global`.
pub(super) fn ctl_var_store(scope: VarScope, tab_store: &CtlVarStore) -> &CtlVarStore {
    match scope {
        VarScope::Tab | VarScope::Session => tab_store,
        VarScope::Global => &GLOBAL_CTL_VARS,
    }
}

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
    use rand::RngCore as _;
    use std::fmt::Write as _;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    format!("/tmp/isekai-pipe-ctl-{token}.sock")
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

    #[test]
    fn ctl_var_store_resolves_tab_and_session_to_the_tab_store() {
        let tab_store = CtlVarStore::new();
        tab_store.set("k", "tab-value");
        assert_eq!(ctl_var_store(VarScope::Tab, &tab_store).get("k"), Some("tab-value".to_string()));
        assert_eq!(ctl_var_store(VarScope::Session, &tab_store).get("k"), Some("tab-value".to_string()));
    }

    #[test]
    fn ctl_var_store_resolves_global_to_the_shared_process_wide_store_across_distinct_tab_stores() {
        let tab_store_a = CtlVarStore::new();
        let tab_store_b = CtlVarStore::new();
        // Use a key unlikely to collide with other tests sharing this process-wide static.
        ctl_var_store(VarScope::Global, &tab_store_a).set("ctl_var_store_global_test_key", "global-value");
        assert_eq!(
            ctl_var_store(VarScope::Global, &tab_store_b).get("ctl_var_store_global_test_key"),
            Some("global-value".to_string()),
            "Global scope must be visible from a different tab's store reference"
        );
    }
}
