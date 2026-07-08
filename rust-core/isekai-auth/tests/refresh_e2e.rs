//! End-to-end tests for `isekai_auth::refresh` (RFC 6749 §6 `refresh_token`
//! grant) and, more importantly, for `FileTokenProvider::get_relay_jwt`'s
//! auto-refresh path (`archive/ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー":
//! "保存済みトークンの`expires_at`が近い/過ぎている場合、`refresh_token`を
//! 使って自動的にリフレッシュを試みる"). Uses the same hand-rolled
//! `std::net::TcpListener`-based mock HTTP server as `device_flow_e2e.rs`
//! (see that file's module docs for why no server crate is pulled in);
//! duplicated here rather than factored into a shared test-support module,
//! matching `isekai-ssh`'s own established convention of one self-contained
//! file per test scenario.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use isekai_auth::{refresh::refresh_access_token, FileTokenProvider, TokenProvider, TokenSet};

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

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[test]
fn refresh_access_token_exchanges_a_refresh_token_for_a_new_access_token() {
    let base_url = spawn_mock_oauth_server(|path, body| {
        assert_eq!(path, "/token");
        assert!(body.contains("grant_type=refresh_token"), "unexpected refresh body: {body}");
        assert!(body.contains("refresh_token=old-refresh-token"), "unexpected refresh body: {body}");
        assert!(body.contains("client_id=test-client"), "unexpected refresh body: {body}");
        (200, r#"{"access_token":"new-access-token","refresh_token":"new-refresh-token","expires_in":7200}"#.to_string())
    });

    let response = refresh_access_token(&format!("{base_url}/token"), Some("test-client"), "old-refresh-token")
        .expect("refresh_access_token failed");
    assert_eq!(response.access_token, "new-access-token");
    assert_eq!(response.refresh_token.as_deref(), Some("new-refresh-token"));
    assert_eq!(response.expires_in, Some(7200));
}

#[test]
fn refresh_access_token_surfaces_invalid_grant() {
    let base_url =
        spawn_mock_oauth_server(|_path, _body| (400, r#"{"error":"invalid_grant","error_description":"expired"}"#.to_string()));

    let err = refresh_access_token(&format!("{base_url}/token"), Some("test-client"), "a-stale-refresh-token")
        .unwrap_err();
    assert!(matches!(
        err,
        isekai_auth::AuthError::OAuthError { grant: "refresh_token", error, .. } if error == "invalid_grant"
    ));
}

/// The acceptance-critical scenario (`archive/ISEKAI_SSH_DESIGN.md` "JWT発行・配布
/// フロー"): a token file whose `expires_at` is already in the past gets
/// transparently refreshed by `FileTokenProvider::get_relay_jwt`, and the
/// refreshed token (including the rotated refresh token) is persisted back
/// to disk — a second read sees the new token, not the stale one.
#[test]
fn file_token_provider_auto_refreshes_an_expired_token_via_the_mock_token_endpoint() {
    let base_url = spawn_mock_oauth_server(|path, body| {
        assert_eq!(path, "/token");
        assert!(body.contains("grant_type=refresh_token"), "unexpected refresh body: {body}");
        assert!(body.contains("refresh_token=old-refresh-token"), "unexpected refresh body: {body}");
        (
            200,
            r#"{"access_token":"refreshed-access-token","refresh_token":"rotated-refresh-token","expires_in":3600}"#
                .to_string(),
        )
    });

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token.json");
    let expired = TokenSet {
        access_token: "stale-access-token".to_string(),
        refresh_token: Some("old-refresh-token".to_string()),
        expires_at: Some(0), // 1970-01-01 — unambiguously in the past.
        token_endpoint: Some(format!("{base_url}/token")),
        client_id: Some("test-client".to_string()),
    };
    isekai_auth::save_token_set(&path, &expired).unwrap();

    let provider = FileTokenProvider::new(path.clone());
    let jwt = provider.get_relay_jwt().expect("get_relay_jwt should auto-refresh");
    assert_eq!(jwt, "refreshed-access-token");

    // The refresh must have been persisted: a fresh read off disk sees the
    // new token, not the stale one, and its refresh token was rotated.
    let updated = isekai_auth::load_token_set(&path).unwrap();
    assert_eq!(updated.access_token, "refreshed-access-token");
    assert_eq!(updated.refresh_token.as_deref(), Some("rotated-refresh-token"));
    assert_eq!(updated.token_endpoint.as_deref(), Some(format!("{base_url}/token").as_str()));
    assert!(updated.expires_at.unwrap() > now_unix(), "refreshed token should expire in the future");
    assert!(!updated.needs_refresh(), "freshly refreshed token should not immediately need another refresh");

    // A second `get_relay_jwt()` call should just return the now-valid
    // token without hitting the (now torn-down expectations of the) mock
    // server again.
    let jwt_again = provider.get_relay_jwt().unwrap();
    assert_eq!(jwt_again, "refreshed-access-token");
}
