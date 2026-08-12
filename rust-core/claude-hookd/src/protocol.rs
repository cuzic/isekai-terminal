//! `CtlMessage`/`ProgressState`/`NotifyKind` for the `IsekaiPipeCtl` delivery
//! mode ([`super::delivery`]) — duplicated from (no longer depends on)
//! `isekai-protocol`, the same "deliberately independent, kept in sync
//! manually" trade-off `delivery.rs::send_ctl_message`'s own doc comment
//! already documents for the wire-protocol function itself. If the real
//! `isekai-protocol::ctl` wire format ever changes, this module needs
//! updating to match.
//!
//! Only the subset claude-hookd actually constructs and sends is copied —
//! `CtlMessage` has several other variants (clipboard sync, shared
//! variables, remote-build streaming) that only `isekai-pipe`/`isekai-ssh`
//! use, and `NotifyKind` has several other variants only tmux hooks
//! (`Bell`/`Activity`/`Silence`/`JobDone`) construct. `Deserialize` is kept
//! even though claude-hookd only ever serializes and sends — this crate's
//! own tests round-trip-decode what was sent to pin the wire bytes.

use serde::{Deserialize, Serialize};

/// One message sent over a tab's control-plane UNIX domain socket (host →
/// device only, from this crate's perspective — it never decodes an
/// incoming message for real, only in tests).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub(crate) enum CtlMessage {
    #[serde(rename = "notify")]
    Notify {
        kind: NotifyKind,
        #[serde(default)]
        tmux_tag: String,
        #[serde(default)]
        seq: u64,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
    },
    #[serde(rename = "tab_color")]
    SetTabColor { r: u8, g: u8, b: u8 },
    #[serde(rename = "progress")]
    SetProgress { state: ProgressState, progress: u8 },
}

/// Reason a `Notify` fired — claude-hookd only ever constructs `Waiting`
/// (a Claude Code `Notification` hook firing for a permission prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NotifyKind {
    #[serde(rename = "waiting")]
    Waiting,
}

/// Progress-bar state for `CtlMessage::SetProgress`, matching the
/// ConEmu-originated `OSC 9;4;<state>;<progress>BEL` convention (tab icon
/// progress ring + taskbar integration on terminals that support it, a
/// harmless no-op elsewhere). The numeric values are the actual OSC 9;4
/// wire values, not an arbitrary internal choice — [`super::tab_progress`]
/// casts `state as u8` directly into the escape sequence, so these must
/// stay byte-for-byte identical to `isekai-protocol::ctl::ProgressState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ProgressState {
    #[serde(rename = "none")]
    None = 0,
    #[serde(rename = "normal")]
    Normal = 1,
    #[serde(rename = "error")]
    Error = 2,
    #[serde(rename = "indeterminate")]
    Indeterminate = 3,
    #[serde(rename = "warning")]
    Warning = 4,
}
