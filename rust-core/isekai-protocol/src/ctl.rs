//! Wire types for the remote→local/local→remote control-plane
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic M): tab title changes, clipboard sync,
//! and (task #16) a scoped shared-variable KV store, all carried over a
//! per-tab UNIX domain socket forwarded alongside the SSH session rather
//! than over the shared isekai-transport connection (which cannot
//! distinguish tabs once SSH ControlMaster/connection pooling shares one
//! connection across several of them).
//!
//! `SetVar`/`GetVarRequest`/`GetVarResponse` (task #16, isekai-terminal
//! tracker: "isekai-pipe ctl に setvar/getvar・file系スクリプタブルコマンドを
//! 追加") reuse the exact same request/response shape `ClipboardPullRequest`/
//! `ClipboardPullResponse` already established: `SetVar` is fire-and-forget
//! like `SetTitle`, `GetVarRequest` expects a `GetVarResponse` on the same
//! connection like `ClipboardPullRequest` does. This module only defines the
//! wire format and validation; the actual KV store (an in-memory
//! `HashMap<String, String>` per scope, intentionally simple —
//! process-memory that dies with the hosting session/process, no disk
//! persistence in this first cut) is owned by whichever process hosts the
//! receiving end of the channel (`isekai-ssh`'s CLI wrapper or the
//! isekai-terminal Android app's in-process listener), not by this crate.
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

/// Cap on a `setvar`/`getvar` key's byte length.
pub const MAX_VAR_KEY_LEN: usize = 256;

/// Cap on a `setvar` value's byte length. Shares `MAX_CLIPBOARD_TEXT_DECODED_LEN`'s
/// order of magnitude deliberately: vars are meant for short status strings/
/// short-lived tokens (task #16's motivating examples: a build status string,
/// a short-lived auth token), not bulk data transfer — that's what `file cat`
/// (chunked, no such cap) is for.
pub const MAX_VAR_VALUE_LEN: usize = 64 * 1024;

/// Scope a `setvar`/`getvar` key is stored/looked up under. Resolved by
/// whichever process hosts the receiving end of the ctl-socket-forward
/// channel (the `isekai-ssh` CLI wrapper, or the isekai-terminal Android app's
/// in-process listener) — see that process's own `CtlVarStore` instance(s).
///
/// `Tab` and `Session` currently resolve to the *same* store (there is no
/// isekai-terminal concept of "multiple sessions sharing one tab" yet, unlike
/// Wave Terminal's block/tab/client hierarchy this feature is modeled after)
/// — kept as distinct wire values for forward-compatibility rather than
/// collapsed into one, so a future sub-tab-session concept doesn't need a
/// wire-format change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VarScope {
    #[serde(rename = "tab")]
    Tab,
    #[serde(rename = "session")]
    Session,
    #[serde(rename = "global")]
    Global,
}

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
    /// host → device: store `value` under `key` in the given scope's shared
    /// KV store (task #16, `isekai-pipe ctl setvar`). Fire-and-forget, same
    /// as `SetTitle` — no response is expected or sent.
    #[serde(rename = "setvar")]
    SetVar {
        scope: VarScope,
        key: String,
        value: String,
    },
    /// host → device: ask for the current value of `key` in the given scope
    /// (task #16, `isekai-pipe ctl getvar`). Expects a `GetVarResponse` on
    /// the same connection, same request/response pattern as
    /// `ClipboardPullRequest`/`ClipboardPullResponse`.
    #[serde(rename = "getvar_request")]
    GetVarRequest { scope: VarScope, key: String },
    /// device → host: reply to `GetVarRequest`. `value` is `None` if the key
    /// was never set in that scope (distinct from an empty string, which is
    /// a valid stored value).
    #[serde(rename = "getvar_response")]
    GetVarResponse { value: Option<String> },
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
        CtlMessage::SetVar { key, value, .. } => {
            validate_var_key(key)?;
            if value.len() > MAX_VAR_VALUE_LEN {
                return Err(ProtocolError::CtlMessageField {
                    field: "value",
                    reason: format!(
                        "is {} bytes, exceeding the {MAX_VAR_VALUE_LEN} byte limit",
                        value.len()
                    ),
                });
            }
            Ok(())
        }
        CtlMessage::GetVarRequest { key, .. } => validate_var_key(key),
        CtlMessage::GetVarResponse { value } => {
            if let Some(value) = value {
                if value.len() > MAX_VAR_VALUE_LEN {
                    return Err(ProtocolError::CtlMessageField {
                        field: "value",
                        reason: format!(
                            "is {} bytes, exceeding the {MAX_VAR_VALUE_LEN} byte limit",
                            value.len()
                        ),
                    });
                }
            }
            Ok(())
        }
    }
}

fn validate_var_key(key: &str) -> Result<(), ProtocolError> {
    if key.is_empty() {
        return Err(ProtocolError::CtlMessageField {
            field: "key",
            reason: "must be non-empty".to_string(),
        });
    }
    if key.len() > MAX_VAR_KEY_LEN {
        return Err(ProtocolError::CtlMessageField {
            field: "key",
            reason: format!("is {} bytes, exceeding the {MAX_VAR_KEY_LEN} byte limit", key.len()),
        });
    }
    Ok(())
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

    #[test]
    fn decodes_setvar_for_each_scope() {
        for (scope_json, scope) in [
            ("tab", VarScope::Tab),
            ("session", VarScope::Session),
            ("global", VarScope::Global),
        ] {
            let json = format!(r#"{{"op":"setvar","scope":"{scope_json}","key":"last_build_status","value":"ok"}}"#);
            let msg = decode_ctl_message(json.as_bytes()).unwrap();
            assert_eq!(
                msg,
                CtlMessage::SetVar {
                    scope,
                    key: "last_build_status".to_string(),
                    value: "ok".to_string(),
                }
            );
        }
    }

    #[test]
    fn rejects_setvar_with_empty_key() {
        let json = br#"{"op":"setvar","scope":"tab","key":"","value":"x"}"#;
        let err = decode_ctl_message(json).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageField { field: "key", .. }));
    }

    #[test]
    fn rejects_setvar_with_oversized_key() {
        let key = "k".repeat(MAX_VAR_KEY_LEN + 1);
        let json = format!(r#"{{"op":"setvar","scope":"tab","key":"{key}","value":"x"}}"#);
        let err = decode_ctl_message(json.as_bytes()).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageField { field: "key", .. }));
    }

    #[test]
    fn rejects_setvar_with_oversized_value() {
        let value = "v".repeat(MAX_VAR_VALUE_LEN + 1);
        let json = format!(r#"{{"op":"setvar","scope":"global","key":"k","value":"{value}"}}"#);
        let err = decode_ctl_message(json.as_bytes()).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageField { field: "value", .. }));
    }

    #[test]
    fn decodes_getvar_request_and_response() {
        let request = br#"{"op":"getvar_request","scope":"global","key":"last_build_status"}"#;
        assert_eq!(
            decode_ctl_message(request).unwrap(),
            CtlMessage::GetVarRequest {
                scope: VarScope::Global,
                key: "last_build_status".to_string(),
            }
        );

        let response = br#"{"op":"getvar_response","value":"ok"}"#;
        assert_eq!(
            decode_ctl_message(response).unwrap(),
            CtlMessage::GetVarResponse { value: Some("ok".to_string()) }
        );

        let empty_response = br#"{"op":"getvar_response","value":null}"#;
        assert_eq!(
            decode_ctl_message(empty_response).unwrap(),
            CtlMessage::GetVarResponse { value: None }
        );
    }

    #[test]
    fn rejects_getvar_request_with_empty_key() {
        let json = br#"{"op":"getvar_request","scope":"tab","key":""}"#;
        let err = decode_ctl_message(json).unwrap_err();
        assert!(matches!(err, ProtocolError::CtlMessageField { field: "key", .. }));
    }
}
