//! End-to-end tests for `isekai-ssh login`/`logout`
//! (`archive/ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー", フェーズ分割案 S-5).
//!
//! Spawns the real compiled `isekai-ssh` binary (not a library-level call —
//! this crate has no `[lib]` target, only `[[bin]]`, matching
//! `connect_e2e.rs`/`init_e2e.rs`'s own convention) against a real Device
//! Authorization Grant mock server on `127.0.0.1`, and inspects the actual
//! `~/.config/isekai-ssh/token.json` file it writes under a fake `$HOME`
//! (same `HOME` env override trick `init_e2e.rs`/`trust_store_e2e.rs` use
//! for the trust store file).
//!
//! ## Why a hand-rolled HTTP/1.1 mock server, not a server crate
//!
//! Mirrors `isekai-auth/tests/device_flow_e2e.rs`'s own choice (see that
//! file's module docs for the full reasoning): a server crate would drag in
//! a whole async HTTP stack just to answer two endpoints that only need to
//! read a POST body and write back JSON. `tokio::net::TcpListener` is used
//! here (rather than `std::net::TcpListener` as in `isekai-auth`'s copy)
//! since this crate already depends on tokio for its own async tests.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command as TokioCommand;

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

/// Starts a background-task HTTP/1.1 server that answers every request by
/// calling `handler(path, body)` and returns `(status, json_body)`. Returns
/// the server's listen address.
async fn spawn_mock_oauth_server<F>(handler: F) -> SocketAddr
where
    F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("failed to bind mock OAuth server");
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = handle_one_request(stream, &*handler).await;
            });
        }
    });

    addr
}

async fn handle_one_request(
    mut stream: TcpStream,
    handler: &(dyn Fn(&str, &str) -> (u16, String) + Send + Sync),
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let headers_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
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
        let n = stream.read(&mut chunk).await?;
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
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn login_completes_and_writes_the_token_file_after_pending_polls() {
    let poll_count = Arc::new(AtomicUsize::new(0));
    let poll_count_for_handler = poll_count.clone();

    let addr = spawn_mock_oauth_server(move |path, body| match path {
        "/device_authorization" => {
            assert!(body.contains("client_id=isekai-ssh-test-client"), "unexpected body: {body}");
            (
                200,
                r#"{"device_code":"DEVCODE123","user_code":"WDJB-MJHT","verification_uri":"https://example.com/device","verification_uri_complete":"https://example.com/device?user_code=WDJB-MJHT","expires_in":60,"interval":1}"#
                    .to_string(),
            )
        }
        "/token" => {
            assert!(body.contains("device_code=DEVCODE123"), "unexpected body: {body}");
            let attempt = poll_count_for_handler.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                (400, r#"{"error":"authorization_pending"}"#.to_string())
            } else {
                (
                    200,
                    r#"{"access_token":"e2e-access-token","refresh_token":"e2e-refresh-token","expires_in":3600}"#
                        .to_string(),
                )
            }
        }
        other => panic!("unexpected request path {other}"),
    })
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = TokioCommand::new(isekai_ssh_bin_path())
        .arg("login")
        .arg("--device-auth-endpoint")
        .arg(format!("http://{addr}/device_authorization"))
        .arg("--token-endpoint")
        .arg(format!("http://{addr}/token"))
        .arg("--client-id")
        .arg("isekai-ssh-test-client")
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to run isekai-ssh login");

    assert!(
        output.status.success(),
        "isekai-ssh login exited with {:?}; stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("https://example.com/device?user_code=WDJB-MJHT"), "stdout was: {stdout}");
    assert!(stdout.contains("Logged in"), "stdout was: {stdout}");

    let token_path = home.join(".config").join("isekai-ssh").join("token.json");
    assert!(token_path.exists(), "expected token file at {token_path:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file should be 0600");
    }

    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&token_path).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "e2e-access-token");
    assert_eq!(saved["refresh_token"], "e2e-refresh-token");
    assert_eq!(saved["token_endpoint"], format!("http://{addr}/token"));
    assert_eq!(saved["client_id"], "isekai-ssh-test-client");
    let expires_at = saved["expires_at"].as_i64().expect("expires_at should be an integer");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((now..=now + 3700).contains(&expires_at), "expires_at {expires_at} not close to now+3600 ({now})");

    assert_eq!(poll_count.load(Ordering::SeqCst), 3, "expected exactly 2 pending polls then 1 success");
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_removes_the_token_file() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config_dir = home.join(".config").join("isekai-ssh");
    std::fs::create_dir_all(&config_dir).unwrap();
    let token_path = config_dir.join("token.json");
    std::fs::write(&token_path, r#"{"relay_jwt": "a-token"}"#).unwrap();
    assert!(token_path.exists());

    let output = TokioCommand::new(isekai_ssh_bin_path())
        .arg("logout")
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to run isekai-ssh logout");

    assert!(
        output.status.success(),
        "isekai-ssh logout exited with {:?}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!token_path.exists(), "token file should have been removed");
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_is_not_an_error_when_already_logged_out() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let token_path = home.join(".config").join("isekai-ssh").join("token.json");
    assert!(matches!(std::fs::metadata(&token_path), Err(e) if e.kind() == ErrorKind::NotFound));

    let output = TokioCommand::new(isekai_ssh_bin_path())
        .arg("logout")
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to run isekai-ssh logout");

    assert!(
        output.status.success(),
        "isekai-ssh logout (already logged out) exited with {:?}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
