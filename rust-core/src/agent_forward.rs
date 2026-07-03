//! SSH agent forwarding: 自前実装の最小プロトコルサブセット。
//!
//! `russh_keys::agent::server::serve()` は空の `KeyStore` を前提としており、
//! 「常に1本の既存 `PrivateKey`（このセッションの認証に使ったのと同じ鍵）だけを
//! 提供する」という用途には合わないため流用しない。ここでは SSH agent protocol
//! （draft-miller-ssh-agent）のうち、以下のメッセージだけを自前実装する:
//!
//! - `SSH2_AGENTC_REQUEST_IDENTITIES` (11) → `SSH2_AGENT_IDENTITIES_ANSWER` (12)
//! - `SSH2_AGENTC_SIGN_REQUEST` (13) → `SSH2_AGENT_SIGN_RESPONSE` (14)
//!
//! それ以外（鍵の追加・削除・ロック等）は `SSH_AGENT_FAILURE` (5) を返す。
//!
//! セキュリティ設計: 署名要求（SIGN_REQUEST）ごとに、呼び出し元
//! （`session.rs` の `session_event_loop`）へ `TransportEvent::AgentSignRequest`
//! を送って oneshot で承認/拒否を待つ。タイムアウトした場合も拒否として扱う。

use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};
use russh::{client, Channel};
use russh_keys::helpers::{sign_workaround, EncodedExt};
use russh_keys::ssh_key::Signature;
use russh_keys::{HashAlg, PrivateKey, PublicKeyBase64};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::transport::TransportEvent;

// ── SSH agent protocol message numbers ───────────────────

const SSH_AGENT_FAILURE: u8 = 5;
const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
const SSH_AGENT_SIGN_RESPONSE: u8 = 14;

/// 単一フレームの最大サイズ（暴走防止のガード。実際の鍵/署名はこれよりずっと小さい）。
const MAX_FRAME_LEN: usize = 256 * 1024;

/// 署名確認の待ち時間。タイムアウトしたら拒否扱い（Kotlin 側の UI がタイムアウトしても
/// Rust 側が永久に待ち続けないようにするための保険）。
const SIGN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

// ── frame (de)serialization helpers ──────────────────────

fn read_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let bytes = buf.get(*pos..*pos + 4)?;
    *pos += 4;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn read_string<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = read_u32(buf, pos)? as usize;
    let s = buf.get(*pos..*pos + len)?;
    *pos += len;
    Some(s)
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

fn build_failure() -> Vec<u8> {
    vec![SSH_AGENT_FAILURE]
}

/// REQUEST_IDENTITIES への応答。転送する鍵が無ければ0件で応答する。
fn build_identities_answer(key: Option<&Arc<PrivateKey>>) -> Vec<u8> {
    let mut out = vec![SSH_AGENT_IDENTITIES_ANSWER];
    match key {
        Some(k) => {
            write_u32(&mut out, 1);
            write_string(&mut out, &k.public_key_bytes());
            write_string(&mut out, b""); // comment
        }
        None => write_u32(&mut out, 0),
    }
    out
}

/// SIGN_RESPONSE への応答本体を組み立てる。
fn build_sign_response(sig: &Signature) -> Vec<u8> {
    let mut out = vec![SSH_AGENT_SIGN_RESPONSE];
    let encoded = sig.encoded().unwrap_or_default();
    write_string(&mut out, &encoded);
    out
}

/// `SIGN_REQUEST` (opcode を除いたペイロード) をパースする。
/// `flags` フィールドはこの最小実装では無視する。
struct SignRequest<'a> {
    key_blob: &'a [u8],
    data: &'a [u8],
}

fn parse_sign_request(payload: &[u8]) -> Option<SignRequest<'_>> {
    let mut pos = 0;
    let key_blob = read_string(payload, &mut pos)?;
    let data = read_string(payload, &mut pos)?;
    Some(SignRequest { key_blob, data })
}

// ── message dispatch (pure, testable) ────────────────────

/// 1メッセージ分（opcode 込みのペイロード、外側の4byte長は含まない）を処理して
/// 応答ペイロード（これも外側の4byte長は含まない）を返す。
async fn handle_message(
    payload: &[u8],
    key: Option<&Arc<PrivateKey>>,
    event_tx: &mpsc::Sender<TransportEvent>,
) -> Vec<u8> {
    match payload.first() {
        Some(&SSH_AGENTC_REQUEST_IDENTITIES) => build_identities_answer(key),
        Some(&SSH_AGENTC_SIGN_REQUEST) => {
            handle_sign_request(&payload[1..], key, event_tx).await
        }
        _ => build_failure(),
    }
}

async fn handle_sign_request(
    rest: &[u8],
    key: Option<&Arc<PrivateKey>>,
    event_tx: &mpsc::Sender<TransportEvent>,
) -> Vec<u8> {
    let Some(key) = key else {
        return build_failure();
    };
    let Some(req) = parse_sign_request(rest) else {
        return build_failure();
    };
    if req.key_blob != key.public_key_bytes().as_slice() {
        warn!("agent_forward: sign request for unknown key blob, rejecting");
        return build_failure();
    }

    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    if !confirm_sign(&fingerprint, event_tx).await {
        debug!("agent_forward: sign request for {} denied", fingerprint);
        return build_failure();
    }

    match sign_workaround(key, req.data) {
        Ok(sig) => build_sign_response(&sig),
        Err(e) => {
            warn!("agent_forward: signing failed: {}", e);
            build_failure()
        }
    }
}

/// ユーザー確認（署名要求ごと）を行う。呼び出し元がイベントを受け取れない・
/// タイムアウトした場合は拒否扱い。
async fn confirm_sign(fingerprint: &str, event_tx: &mpsc::Sender<TransportEvent>) -> bool {
    let (reply_tx, reply_rx) = oneshot::channel();
    if event_tx
        .send(TransportEvent::AgentSignRequest {
            key_fingerprint: fingerprint.to_string(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return false;
    }
    matches!(
        tokio::time::timeout(SIGN_CONFIRM_TIMEOUT, reply_rx).await,
        Ok(Ok(true))
    )
}

// ── stream loop ───────────────────────────────────────────

/// 1つの agent-forward チャネル（ストリーム）を最初から最後まで処理する。
/// 汎用の `AsyncRead + AsyncWrite` を受け取るので、実 russh の `Channel` からも
/// テスト用の `tokio::io::duplex` からも同じロジックで検証できる。
pub(crate) async fn serve_agent_stream<S>(
    mut stream: S,
    key: Option<Arc<PrivateKey>>,
    event_tx: mpsc::Sender<TransportEvent>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > MAX_FRAME_LEN {
            warn!("agent_forward: invalid frame length {}", len);
            break;
        }
        let mut payload = vec![0u8; len];
        if stream.read_exact(&mut payload).await.is_err() {
            break;
        }

        let response = handle_message(&payload, key.as_ref(), &event_tx).await;

        let mut out = Vec::with_capacity(4 + response.len());
        write_u32(&mut out, response.len() as u32);
        out.extend_from_slice(&response);
        if stream.write_all(&out).await.is_err() {
            break;
        }
    }
    debug!("agent_forward: channel closed");
}

/// russh がサーバーから開き返してきた agent-forward チャネルを処理する。
pub(crate) async fn serve_agent_channel(
    channel: Channel<client::Msg>,
    key: Option<Arc<PrivateKey>>,
    event_tx: mpsc::Sender<TransportEvent>,
) {
    serve_agent_stream(channel.into_stream(), key, event_tx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_keys::signature::Verifier;
    use russh_keys::ssh_encoding::Decode;

    // `ssh-keygen -t ed25519 -N ""` で生成したテスト専用の使い捨て鍵。
    const TEST_KEY_PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACAOWS3NCRUeEdqfLK0J24wnp1WOKG+uZkmr1CmrZ/jSggAAAJhQVq2jUFat\n\
owAAAAtzc2gtZWQyNTUxOQAAACAOWS3NCRUeEdqfLK0J24wnp1WOKG+uZkmr1CmrZ/jSgg\n\
AAAEBxRrC4MhD3YdjfyaMpLOpigSH1N0iQvsJnUrYgz39/Cg5ZLc0JFR4R2p8srQnbjCen\n\
VY4ob65mSavUKatn+NKCAAAAEnRlc3QtYWdlbnQtZm9yd2FyZAECAw==\n\
-----END OPENSSH PRIVATE KEY-----\n";

    fn test_key() -> Arc<PrivateKey> {
        Arc::new(PrivateKey::from_openssh(TEST_KEY_PEM).expect("valid test key"))
    }

    // `test_key()` とは別のダミー ed25519 鍵（"unknown key blob" ケースの検証用）。
    const OTHER_KEY_PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACDu9KcgkTYMdAQwdKjjgQGgXwULyFPtmymVHClyfeVJPQAAAJhSUjZAUlI2\n\
QAAAAAtzc2gtZWQyNTUxOQAAACDu9KcgkTYMdAQwdKjjgQGgXwULyFPtmymVHClyfeVJPQ\n\
AAAEDIUmUaNeBe8YpDTdHVFIDfDyWERfUw2ge9mRksddPcTO70pyCRNgx0BDB0qOOBAaBf\n\
BQvIU+2bKZUcKXJ95Uk9AAAAFHRlc3QtYWdlbnQtZm9yd2FyZC0yAQ==\n\
-----END OPENSSH PRIVATE KEY-----\n";

    fn other_key() -> Arc<PrivateKey> {
        Arc::new(PrivateKey::from_openssh(OTHER_KEY_PEM).expect("valid other test key"))
    }

    #[test]
    fn read_write_string_roundtrip() {
        let mut out = Vec::new();
        write_string(&mut out, b"hello");
        let mut pos = 0;
        assert_eq!(read_string(&out, &mut pos), Some(&b"hello"[..]));
        assert_eq!(pos, out.len());
    }

    #[test]
    fn read_string_rejects_truncated_input() {
        let mut out = Vec::new();
        write_u32(&mut out, 10); // claims 10 bytes but body is empty
        let mut pos = 0;
        assert_eq!(read_string(&out, &mut pos), None);
    }

    #[test]
    fn build_identities_answer_empty_when_no_key() {
        let out = build_identities_answer(None);
        assert_eq!(out, vec![SSH_AGENT_IDENTITIES_ANSWER, 0, 0, 0, 0]);
    }

    #[test]
    fn build_identities_answer_contains_key_blob() {
        let key = test_key();
        let out = build_identities_answer(Some(&key));
        assert_eq!(out[0], SSH_AGENT_IDENTITIES_ANSWER);
        let mut pos = 1;
        let count = read_u32(&out, &mut pos).unwrap();
        assert_eq!(count, 1);
        let blob = read_string(&out, &mut pos).unwrap();
        assert_eq!(blob, key.public_key_bytes().as_slice());
    }

    #[test]
    fn parse_sign_request_extracts_blob_and_data() {
        let mut payload = Vec::new();
        write_string(&mut payload, b"blob");
        write_string(&mut payload, b"data-to-sign");
        write_u32(&mut payload, 0); // flags
        let req = parse_sign_request(&payload).unwrap();
        assert_eq!(req.key_blob, b"blob");
        assert_eq!(req.data, b"data-to-sign");
    }

    #[tokio::test]
    async fn handle_message_unknown_opcode_fails() {
        let (tx, _rx) = mpsc::channel(1);
        let out = handle_message(&[99], None, &tx).await;
        assert_eq!(out, vec![SSH_AGENT_FAILURE]);
    }

    #[tokio::test]
    async fn handle_message_request_identities_no_key() {
        let (tx, _rx) = mpsc::channel(1);
        let out = handle_message(&[SSH_AGENTC_REQUEST_IDENTITIES], None, &tx).await;
        assert_eq!(out, build_identities_answer(None));
    }

    #[tokio::test]
    async fn sign_request_denied_returns_failure() {
        let key = test_key();
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            if let Some(TransportEvent::AgentSignRequest { reply, .. }) = rx.recv().await {
                let _ = reply.send(false);
            }
        });

        let mut payload = vec![SSH_AGENTC_SIGN_REQUEST];
        write_string(&mut payload, &key.public_key_bytes());
        write_string(&mut payload, b"some data");
        write_u32(&mut payload, 0);

        let out = handle_message(&payload, Some(&key), &tx).await;
        assert_eq!(out, vec![SSH_AGENT_FAILURE]);
    }

    #[tokio::test]
    async fn sign_request_approved_produces_valid_signature() {
        let key = test_key();
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            if let Some(TransportEvent::AgentSignRequest { reply, .. }) = rx.recv().await {
                let _ = reply.send(true);
            }
        });

        let data_to_sign = b"the data ssh wants signed";
        let mut payload = vec![SSH_AGENTC_SIGN_REQUEST];
        write_string(&mut payload, &key.public_key_bytes());
        write_string(&mut payload, data_to_sign);
        write_u32(&mut payload, 0);

        let out = handle_message(&payload, Some(&key), &tx).await;
        assert_eq!(out[0], SSH_AGENT_SIGN_RESPONSE);

        let mut pos = 1;
        let sig_bytes = read_string(&out, &mut pos).unwrap();
        let mut reader = sig_bytes;
        let sig = Signature::decode(&mut reader).expect("signature must decode");
        // `PublicKey` には namespace 付きの SSHSIG 用 `verify()` 固有メソッドもあり
        // 名前が衝突するため、`Verifier` トレイト経由であることを明示して呼ぶ。
        Verifier::verify(key.public_key(), data_to_sign, &sig)
            .expect("signature must verify against the public key");
    }

    #[tokio::test]
    async fn sign_request_rejects_mismatched_key_blob() {
        let key = test_key();
        let other = other_key();
        let (tx, _rx) = mpsc::channel(1);

        let mut payload = vec![SSH_AGENTC_SIGN_REQUEST];
        write_string(&mut payload, &other.public_key_bytes());
        write_string(&mut payload, b"data");
        write_u32(&mut payload, 0);

        let out = handle_message(&payload, Some(&key), &tx).await;
        assert_eq!(out, vec![SSH_AGENT_FAILURE]);
    }

    #[tokio::test]
    async fn serve_agent_stream_full_roundtrip_over_duplex() {
        let key = test_key();
        let (mut client_side, server_side) = tokio::io::duplex(4096);
        let (tx, mut rx) = mpsc::channel(4);
        tokio::spawn(async move {
            while let Some(TransportEvent::AgentSignRequest { reply, .. }) = rx.recv().await {
                let _ = reply.send(true);
            }
        });

        tokio::spawn(serve_agent_stream(server_side, Some(key.clone()), tx));

        // REQUEST_IDENTITIES
        let mut req = vec![SSH_AGENTC_REQUEST_IDENTITIES];
        let mut frame = Vec::new();
        write_u32(&mut frame, req.len() as u32);
        frame.append(&mut req);
        client_side.write_all(&frame).await.unwrap();

        let mut len_buf = [0u8; 4];
        client_side.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut resp = vec![0u8; len];
        client_side.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp[0], SSH_AGENT_IDENTITIES_ANSWER);
    }
}
