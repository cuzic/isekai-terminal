//! Wire types for the remote→local/local→remote control-plane
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic M): tab title changes and clipboard
//! sync, carried over a per-tab UNIX domain socket forwarded alongside the
//! SSH session rather than over the shared isekai-transport connection
//! (which cannot distinguish tabs once SSH ControlMaster/connection
//! pooling shares one connection across several of them).
//!
//! One `CtlMessage` per line, same "explicit fields, no legacy duplicates"
//! style as `handshake::HandshakeJson`. `isekai-terminal-core` and
//! `isekai-pipe` share this module unchanged.

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Cap on the raw incoming line before it is even handed to `serde_json`.
/// Generous enough for a base64-encoded `MAX_CLIPBOARD_IMAGE_DECODED_LEN`
/// image plus JSON overhead; exists only to reject a hostile/broken peer
/// that floods the socket instead of sending one well-formed line.
pub const MAX_CTL_MESSAGE_LINE_LEN: usize = 8 * 1024 * 1024;

/// Cap on the *decoded* byte length of a `text/plain` or `text/html`
/// clipboard payload.
pub const MAX_CLIPBOARD_TEXT_DECODED_LEN: usize = 64 * 1024;

/// Cap on the *decoded* byte length of an `image/png` clipboard payload.
pub const MAX_CLIPBOARD_IMAGE_DECODED_LEN: usize = 4 * 1024 * 1024;

/// Cap on the byte length of `Notify`'s `tmux_tag` field. Real tags
/// (`tmux_locator::TmuxTag::new_random`, in the top-level `rust-core`
/// crate) are 32-char lowercase-hex strings; this cap is generous headroom
/// against a hostile/broken peer rather than a tight fit to that format,
/// matching the style of the clipboard payload caps above.
pub const MAX_NOTIFY_TAG_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMime {
    #[serde(rename = "text/plain")]
    TextPlain,
    #[serde(rename = "text/html")]
    TextHtml,
    #[serde(rename = "image/png")]
    ImagePng,
}

impl ClipboardMime {
    fn max_decoded_len(self) -> usize {
        match self {
            ClipboardMime::TextPlain | ClipboardMime::TextHtml => MAX_CLIPBOARD_TEXT_DECODED_LEN,
            ClipboardMime::ImagePng => MAX_CLIPBOARD_IMAGE_DECODED_LEN,
        }
    }
}

/// Reason a `Notify` fired. Modeled as a typed enum rather than a bare
/// string (this crate's existing convention — see `ClipboardMime`): #57's
/// Android-side consumer maps each kind to a distinct notification
/// channel/icon, and a free-form string would make new spellings silently
/// fall through to "unrecognized" instead of failing to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyKind {
    /// tmux `bell` hook (BEL / visual bell in a pane).
    #[serde(rename = "bell")]
    Bell,
    /// tmux `alert-activity` hook (output in a `monitor-activity` window).
    #[serde(rename = "activity")]
    Activity,
    /// tmux `alert-silence` hook (a `monitor-silence` timeout elapsed with
    /// no output).
    #[serde(rename = "silence")]
    Silence,
    /// Not a native tmux hook: emitted by a wrapper script/hook combo that
    /// observes a long-running command's exit (#57's design; out of scope
    /// here).
    #[serde(rename = "job_done")]
    JobDone,
}

/// One message exchanged over a tab's control-plane UNIX domain socket.
/// `ClipboardPush`/`ClipboardPullRequest`/`ClipboardPullResponse` are each
/// independently opt-in on the receiving side (`ISEKAI_PIPE_DESIGN.md`
/// Epic M "セキュリティ"): a peer that never enabled push/pull must reject
/// the corresponding variant rather than silently accepting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum CtlMessage {
    #[serde(rename = "title")]
    SetTitle { value: String },
    /// host → device: write to the device's clipboard.
    #[serde(rename = "clip_push")]
    ClipboardPush {
        mime: ClipboardMime,
        data_b64: String,
    },
    /// host → device: ask the device to send its clipboard contents.
    #[serde(rename = "clip_pull_request")]
    ClipboardPullRequest {},
    /// device → host: reply to `ClipboardPullRequest`.
    #[serde(rename = "clip_pull_response")]
    ClipboardPullResponse {
        mime: ClipboardMime,
        data_b64: String,
    },
    /// host → device: something happened in a tmux pane/window the device
    /// isn't currently viewing (#57: tmux hooks running on the host →
    /// Android notifications; this crate only defines the wire type, #57
    /// wires up the hook script that emits it and the Android
    /// notification it produces). Never sent device → host — there's no
    /// response variant.
    #[serde(rename = "notify")]
    Notify {
        kind: NotifyKind,
        /// The stable tag identifying which pane/window this fired in.
        /// This is the *same* tag string `tmux_locator::TmuxTag` already
        /// mints and stores as a tmux user-option — deliberately not a
        /// new identifier scheme. Carried as a plain `String` rather than
        /// the `TmuxTag` newtype itself: `TmuxTag` lives in the top-level
        /// `rust-core` crate, which depends on `isekai-protocol` (not the
        /// reverse), so reusing it here would be a cyclic dependency.
        /// Callers convert at the boundary, the same way `session.rs`
        /// already converts between `isekai_protocol::ClipboardMime` and
        /// `crate::ClipboardMimeKind`.
        tmux_tag: String,
        /// Sender-maintained counter, monotonically increasing per
        /// `tmux_tag` (not global, and deliberately not wall-clock-based:
        /// host and device don't share a clock, and re-running a test
        /// twice should produce identical bytes). Lets a receiver that
        /// already saw the same `(tmux_tag, seq)` pair drop a duplicate
        /// delivery — tmux can re-fire a hook (e.g. re-attaching a
        /// session re-runs `set-hook`-installed triggers) and #57's
        /// transport may itself retry — without needing any per-connection
        /// state to detect it.
        seq: u64,
    },
}

/// Parses and validates one line of control-plane JSON. Rejects oversized
/// input before handing it to `serde_json` so a hostile/broken peer can't
/// force an unbounded allocation.
pub fn decode_ctl_message(bytes: &[u8]) -> Result<CtlMessage, ProtocolError> {
    if bytes.len() > MAX_CTL_MESSAGE_LINE_LEN {
        return Err(ProtocolError::CtlMessageTooLarge {
            got: bytes.len(),
            max: MAX_CTL_MESSAGE_LINE_LEN,
        });
    }
    let parsed: CtlMessage =
        serde_json::from_slice(bytes).map_err(|e| ProtocolError::CtlMessageJson(e.to_string()))?;
    validate_ctl_message(&parsed)?;
    Ok(parsed)
}

pub fn validate_ctl_message(msg: &CtlMessage) -> Result<(), ProtocolError> {
    match msg {
        CtlMessage::SetTitle { value } => {
            if value.is_empty() {
                return Err(ProtocolError::CtlMessageField {
                    field: "value",
                    reason: "must be non-empty".to_string(),
                });
            }
            Ok(())
        }
        CtlMessage::ClipboardPush { mime, data_b64 }
        | CtlMessage::ClipboardPullResponse { mime, data_b64 } => {
            validate_clipboard_payload(*mime, data_b64)
        }
        CtlMessage::ClipboardPullRequest {} => Ok(()),
        CtlMessage::Notify { tmux_tag, .. } => validate_notify_tag(tmux_tag),
    }
}

fn validate_clipboard_payload(mime: ClipboardMime, data_b64: &str) -> Result<(), ProtocolError> {
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64)
        .map_err(|e| ProtocolError::CtlMessageField {
            field: "data_b64",
            reason: e.to_string(),
        })?;
    let max = mime.max_decoded_len();
    if decoded.len() > max {
        return Err(ProtocolError::CtlMessageField {
            field: "data_b64",
            reason: format!(
                "decodes to {} bytes, exceeding the {max} byte limit for {mime:?}",
                decoded.len()
            ),
        });
    }
    Ok(())
}

fn validate_notify_tag(tmux_tag: &str) -> Result<(), ProtocolError> {
    if tmux_tag.is_empty() {
        return Err(ProtocolError::CtlMessageField {
            field: "tmux_tag",
            reason: "must be non-empty".to_string(),
        });
    }
    if tmux_tag.len() > MAX_NOTIFY_TAG_LEN {
        return Err(ProtocolError::CtlMessageField {
            field: "tmux_tag",
            reason: format!(
                "is {} bytes, exceeding the {MAX_NOTIFY_TAG_LEN} byte limit",
                tmux_tag.len()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_title_json() -> Vec<u8> {
        br#"{"op":"title","value":"my-tab"}"#.to_vec()
    }

    #[test]
    fn decodes_set_title() {
        let msg = decode_ctl_message(&set_title_json()).unwrap();
        assert_eq!(
            msg,
            CtlMessage::SetTitle {
                value: "my-tab".to_string()
            }
        );
    }

    #[test]
    fn rejects_empty_title() {
        let json = br#"{"op":"title","value":""}"#;
        let err = decode_ctl_message(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CtlMessageField { field: "value", .. }
        ));
    }

    #[test]
    fn decodes_clipboard_push_text_plain() {
        let json = br#"{"op":"clip_push","mime":"text/plain","data_b64":"aGVsbG8="}"#;
        let msg = decode_ctl_message(json).unwrap();
        assert_eq!(
            msg,
            CtlMessage::ClipboardPush {
                mime: ClipboardMime::TextPlain,
                data_b64: "aGVsbG8=".to_string(),
            }
        );
    }

    #[test]
    fn decodes_clipboard_push_html_and_image() {
        let html = br#"{"op":"clip_push","mime":"text/html","data_b64":"PGI+aGk8L2I+"}"#;
        assert_eq!(
            decode_ctl_message(html).unwrap(),
            CtlMessage::ClipboardPush {
                mime: ClipboardMime::TextHtml,
                data_b64: "PGI+aGk8L2I+".to_string(),
            }
        );

        let image = br#"{"op":"clip_push","mime":"image/png","data_b64":"aGVsbG8="}"#;
        assert_eq!(
            decode_ctl_message(image).unwrap(),
            CtlMessage::ClipboardPush {
                mime: ClipboardMime::ImagePng,
                data_b64: "aGVsbG8=".to_string(),
            }
        );
    }

    #[test]
    fn rejects_clipboard_push_with_invalid_base64() {
        let json = br#"{"op":"clip_push","mime":"text/plain","data_b64":"not-base64!!"}"#;
        let err = decode_ctl_message(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CtlMessageField {
                field: "data_b64",
                ..
            }
        ));
    }

    #[test]
    fn rejects_clipboard_push_exceeding_text_cap() {
        let oversized = "A".repeat(MAX_CLIPBOARD_TEXT_DECODED_LEN + 1);
        let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, oversized);
        let msg = CtlMessage::ClipboardPush {
            mime: ClipboardMime::TextPlain,
            data_b64,
        };
        let err = validate_ctl_message(&msg).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CtlMessageField {
                field: "data_b64",
                ..
            }
        ));
    }

    #[test]
    fn image_cap_is_larger_than_text_cap() {
        let over_text_cap_but_under_image_cap = "A".repeat(MAX_CLIPBOARD_TEXT_DECODED_LEN + 1);
        let data_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &over_text_cap_but_under_image_cap,
        );
        let msg = CtlMessage::ClipboardPush {
            mime: ClipboardMime::ImagePng,
            data_b64,
        };
        validate_ctl_message(&msg).unwrap();
    }

    #[test]
    fn decodes_clipboard_pull_request_and_response() {
        let request = br#"{"op":"clip_pull_request"}"#;
        assert_eq!(
            decode_ctl_message(request).unwrap(),
            CtlMessage::ClipboardPullRequest {}
        );

        let response = br#"{"op":"clip_pull_response","mime":"text/plain","data_b64":"aGVsbG8="}"#;
        assert_eq!(
            decode_ctl_message(response).unwrap(),
            CtlMessage::ClipboardPullResponse {
                mime: ClipboardMime::TextPlain,
                data_b64: "aGVsbG8=".to_string(),
            }
        );
    }

    #[test]
    fn rejects_oversized_line() {
        let mut json = set_title_json();
        json.extend(std::iter::repeat(b' ').take(MAX_CTL_MESSAGE_LINE_LEN));
        let err = decode_ctl_message(&json).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageTooLarge { .. }));
    }

    #[test]
    fn rejects_malformed_json() {
        let err = decode_ctl_message(b"not json").unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageJson(_)));
    }

    #[test]
    fn rejects_unknown_op() {
        let json = br#"{"op":"delete_everything"}"#;
        let err = decode_ctl_message(json).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageJson(_)));
    }

    // ── Notify (#57 wire type; see module doc on `CtlMessage::Notify`) ──

    #[test]
    fn decodes_notify_bell() {
        let json = br#"{"op":"notify","kind":"bell","tmux_tag":"abc123","seq":0}"#;
        let msg = decode_ctl_message(json).unwrap();
        assert_eq!(
            msg,
            CtlMessage::Notify {
                kind: NotifyKind::Bell,
                tmux_tag: "abc123".to_string(),
                seq: 0,
            }
        );
    }

    #[test]
    fn round_trips_all_notify_kinds() {
        for (kind, rendered) in [
            (NotifyKind::Bell, "bell"),
            (NotifyKind::Activity, "activity"),
            (NotifyKind::Silence, "silence"),
            (NotifyKind::JobDone, "job_done"),
        ] {
            let msg = CtlMessage::Notify { kind, tmux_tag: "tag".to_string(), seq: 7 };
            let encoded = serde_json::to_string(&msg).unwrap();
            assert!(
                encoded.contains(&format!("\"kind\":\"{rendered}\"")),
                "expected {rendered:?} in encoded form, got {encoded}"
            );
            let decoded = decode_ctl_message(encoded.as_bytes()).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn rejects_notify_with_empty_tag() {
        let msg = CtlMessage::Notify { kind: NotifyKind::Bell, tmux_tag: String::new(), seq: 0 };
        let err = validate_ctl_message(&msg).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CtlMessageField { field: "tmux_tag", .. }
        ));
    }

    #[test]
    fn rejects_notify_tag_exceeding_cap() {
        let msg = CtlMessage::Notify {
            kind: NotifyKind::Activity,
            tmux_tag: "a".repeat(MAX_NOTIFY_TAG_LEN + 1),
            seq: 0,
        };
        let err = validate_ctl_message(&msg).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CtlMessageField { field: "tmux_tag", .. }
        ));
    }

    #[test]
    fn accepts_notify_tag_at_cap() {
        let msg = CtlMessage::Notify {
            kind: NotifyKind::Silence,
            tmux_tag: "a".repeat(MAX_NOTIFY_TAG_LEN),
            seq: 0,
        };
        validate_ctl_message(&msg).unwrap();
    }

    #[test]
    fn rejects_unknown_notify_kind() {
        let json = br#"{"op":"notify","kind":"made_up","tmux_tag":"abc123","seq":0}"#;
        let err = decode_ctl_message(json).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageJson(_)));
    }

    /// Simulates a *pre-#63* binary's `CtlMessage` — one without the
    /// `Notify` variant — receiving today's `"op":"notify"` line on the
    /// wire, to pin down exactly how an old decoder fails.
    ///
    /// Epic M's wire contract (`ssh_handler::server_channel_open_forwarded_streamlocal`
    /// doc comment) is "1 connection = 1 message": each `CtlMessage` is
    /// forwarded over its own dedicated streamlocal channel, one
    /// `read_line` is attempted, and the *channel* is closed afterward
    /// regardless of whether decoding succeeded. So when an old decoder
    /// can't recognize `"op":"notify"`, `serde_json` fails with a normal
    /// per-message "unknown variant" error — the same failure category
    /// `rejects_unknown_op` above already covers for any unrecognized
    /// `op` — which the old binary's handler logs
    /// (`warn!("...: malformed ctl message: {e}")`) and drops by closing
    /// that one channel. It cannot corrupt or desync any other message,
    /// tab, or the shared SSH connection, because there is no shared
    /// framing/length-prefix state that spans multiple `CtlMessage`s for
    /// a decode failure to leave inconsistent.
    ///
    /// This is a property of the *existing* one-message-per-connection
    /// design, not something this task added — flagged here as the
    /// documented backward-compat behavior for the new variant rather
    /// than a new mechanism, since the wire format has no "unknown
    /// variant, skip it" framing of its own to fall back on if that
    /// one-message-per-connection contract ever changes.
    #[test]
    fn old_decoder_without_notify_variant_fails_without_desyncing() {
        #[derive(Debug, Deserialize)]
        #[serde(tag = "op")]
        #[allow(dead_code)]
        enum OldCtlMessage {
            #[serde(rename = "title")]
            SetTitle { value: String },
            #[serde(rename = "clip_push")]
            ClipboardPush { mime: ClipboardMime, data_b64: String },
            #[serde(rename = "clip_pull_request")]
            ClipboardPullRequest {},
            #[serde(rename = "clip_pull_response")]
            ClipboardPullResponse { mime: ClipboardMime, data_b64: String },
        }

        let json = br#"{"op":"notify","kind":"bell","tmux_tag":"abc123","seq":0}"#;
        // The "old" decoder can't parse it...
        let err = serde_json::from_slice::<OldCtlMessage>(json).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("unknown variant"));
        // ...while today's decoder (with the new variant) parses the very
        // same bytes just fine — confirming the failure above is purely
        // "old code doesn't know this variant yet", not a malformed
        // message.
        decode_ctl_message(json).unwrap();
    }
}
