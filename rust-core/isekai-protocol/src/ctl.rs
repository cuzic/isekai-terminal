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
}
