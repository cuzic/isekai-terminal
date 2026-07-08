//! End-to-end test for `isekai_auth::device_flow` (RFC 8628 Device
//! Authorization Grant, `archive/ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー", フェーズ
//! 分割案 S-5). Drives `request_device_authorization` + `poll_for_token`
//! against a real (if minimal) HTTP server on `127.0.0.1`, not a type-level
//! mock — this proves the actual HTTP request shapes, JSON decoding, and the
//! `authorization_pending` retry loop all work against real bytes on the
//! wire.
//!
//! ## Why a hand-rolled HTTP/1.1 server instead of a server crate
//!
//! This crate has no async runtime dependency at all (`device_flow`'s client
//! is deliberately blocking — see `oauth`'s module docs), so pulling in
//! something like `axum`/`warp` (which drag in tokio + hyper) just for two
//! test endpoints that only need to read a POST body and write back a JSON
//! response would be a much heavier dependency than the thing being tested.
//! `std::net::TcpListener` + a few lines of manual HTTP/1.1 parsing (request
//! line + `Content-Length` header + body) is enough for both endpoints this
//! test needs.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use isekai_auth::device_flow::{poll_for_token, request_device_authorization, DeviceFlowConfig};

/// Starts a background-thread HTTP/1.1 server that answers every request by
/// calling `handler(path, body)` and returns `(status, json_body)`. Returns
/// the server's base URL (`http://127.0.0.1:<port>`).
fn spawn_mock_oauth_server<F>(handler: F) -> String
where
    F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock OAuth server");
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let handler = handler.clone();
            thread::spawn(move || handle_one_request(&mut stream, &*handler));
        }
    });

    format!("http://{addr}")
}

fn handle_one_request(stream: &mut TcpStream, handler: &(dyn Fn(&str, &str) -> (u16, String) + Send + Sync)) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let headers_end = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..headers_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let content_length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);

    let body_start = headers_end + 4;
    while buf.len() < body_start + content_length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_start..(body_start + content_length).min(buf.len())]).to_string();

    let (status, json_body) = handler(&path, &body);
    let status_line = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        _ => "500 Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json_body.len(),
        json_body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[test]
fn device_flow_completes_after_a_few_authorization_pending_polls() {
    let poll_count = Arc::new(AtomicUsize::new(0));
    let poll_count_for_handler = poll_count.clone();

    let base_url = spawn_mock_oauth_server(move |path, body| match path {
        "/device_authorization" => {
            assert!(body.contains("client_id=test-client"), "unexpected device_authorization body: {body}");
            (
                200,
                r#"{"device_code":"DEVCODE123","user_code":"ABCD-EFGH","verification_uri":"https://example.com/device","verification_uri_complete":"https://example.com/device?user_code=ABCD-EFGH","expires_in":60,"interval":1}"#
                    .to_string(),
            )
        }
        "/token" => {
            assert!(body.contains("device_code=DEVCODE123"), "unexpected token poll body: {body}");
            assert!(body.contains("client_id=test-client"), "unexpected token poll body: {body}");
            let attempt = poll_count_for_handler.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                (400, r#"{"error":"authorization_pending"}"#.to_string())
            } else {
                (
                    200,
                    r#"{"access_token":"issued-access-token","refresh_token":"issued-refresh-token","expires_in":3600}"#
                        .to_string(),
                )
            }
        }
        other => panic!("unexpected request path {other}"),
    });

    let config = DeviceFlowConfig {
        device_authorization_endpoint: format!("{base_url}/device_authorization"),
        token_endpoint: format!("{base_url}/token"),
        client_id: "test-client".to_string(),
    };

    let authz = request_device_authorization(&config).expect("device_authorization request failed");
    assert_eq!(authz.device_code, "DEVCODE123");
    assert_eq!(authz.user_code, "ABCD-EFGH");
    assert_eq!(authz.display_uri(), "https://example.com/device?user_code=ABCD-EFGH");

    let token = poll_for_token(&config, &authz).expect("token polling failed");
    assert_eq!(token.access_token, "issued-access-token");
    assert_eq!(token.refresh_token.as_deref(), Some("issued-refresh-token"));
    assert_eq!(token.expires_in, Some(3600));

    // Two `authorization_pending` responses before the third (successful)
    // attempt — proves the retry loop actually waited and re-polled rather
    // than giving up or succeeding immediately.
    assert_eq!(poll_count.load(Ordering::SeqCst), 3);
}

#[test]
fn device_flow_surfaces_access_denied() {
    let base_url = spawn_mock_oauth_server(|path, _body| match path {
        "/device_authorization" => (
            200,
            r#"{"device_code":"DEVCODE","user_code":"WXYZ","verification_uri":"https://example.com/device","expires_in":60,"interval":1}"#
                .to_string(),
        ),
        "/token" => (400, r#"{"error":"access_denied"}"#.to_string()),
        other => panic!("unexpected request path {other}"),
    });

    let config = DeviceFlowConfig {
        device_authorization_endpoint: format!("{base_url}/device_authorization"),
        token_endpoint: format!("{base_url}/token"),
        client_id: "test-client".to_string(),
    };

    let authz = request_device_authorization(&config).unwrap();
    let err = poll_for_token(&config, &authz).unwrap_err();
    assert!(matches!(err, isekai_auth::AuthError::DeviceFlowDenied));
}
