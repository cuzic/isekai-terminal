//! Shared low-level OAuth2 token-endpoint HTTP plumbing for `device_flow`
//! (RFC 8628) and `refresh` (RFC 6749 §6). Both grants POST an
//! `application/x-www-form-urlencoded` body to a token endpoint and get back
//! the same JSON success shape (`{access_token, refresh_token?, expires_in?}`,
//! RFC 6749 §5.1) and the same JSON error shape
//! (`{error, error_description?}`, RFC 6749 §5.2) on failure, so the HTTP +
//! JSON-decoding boilerplate lives here once instead of being duplicated in
//! both callers.
//!
//! HTTP client: `ureq` (blocking, default `rustls`+`ring` TLS backend — the
//! same combination `isekai-transport`/`isekai-terminal-core` already use elsewhere in
//! this workspace). `TokenProvider::get_relay_jwt` (`lib.rs`) is a plain sync
//! `fn` by design, and `isekai-ssh login` (`device_flow`'s caller) only ever
//! has one request in flight at a time, so an async HTTP stack
//! (reqwest+hyper) would be pure overhead here; `isekai-ssh`'s own tokio
//! runtime wraps these blocking calls in `tokio::task::spawn_blocking`
//! rather than this crate (or its trait) becoming async.

use serde::Deserialize;

use crate::AuthError;

/// RFC 6749 §5.1 successful token response. Shared by the device-code grant
/// (`device_flow::poll_for_token`) and the refresh-token grant
/// (`refresh::refresh_access_token`) — both endpoints return exactly this
/// shape on success.
/// `Debug` redacts both tokens.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds from now until `access_token` expires. `Option` because
    /// RFC 6749 doesn't require the field; a response omitting it is treated
    /// as "no known expiry" (mirrors `TokenSet::expires_at`'s `None` case in
    /// `file_provider.rs`, which then never auto-refreshes on this token).
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// RFC 6749 §5.2 error response body.
#[derive(Debug, Deserialize)]
struct TokenErrorBody {
    #[serde(default = "unknown_error")]
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

fn unknown_error() -> String {
    "unknown_error".to_string()
}

/// POSTs `application/x-www-form-urlencoded` `form` to `url` and returns
/// `(status_code, raw_body)` regardless of status — non-2xx is not turned
/// into `Err` here, since callers interpret it differently: device-flow
/// polling treats `authorization_pending`/`slow_down` (both delivered as
/// HTTP 400 per RFC 8628 §3.5) as "keep waiting", not a fatal error.
///
/// Bounded end-to-end by [`HTTP_TIMEOUT`] (ureq 3's own timeouts all default
/// to "none", so an unresponsive token endpoint used to hang a silent
/// re-bootstrap forever), and refuses a non-`https` endpoint unless it is a
/// loopback address (a refresh token sent in clear text would be exposed to
/// anyone on the path).
pub(crate) fn post_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError> {
    require_secure_endpoint(url)?;
    let mut response = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .send_form(form.iter().cloned())
        .map_err(|source| AuthError::HttpRequest { url: url.to_string(), reason: source.to_string() })?;

    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|source| AuthError::HttpRequest { url: url.to_string(), reason: source.to_string() })?;
    Ok((status, body))
}

/// End-to-end bound (DNS through reading the body) on one token-endpoint
/// request.
pub(crate) const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `https://` is always accepted; `http://` only for a loopback host
/// (`localhost`, `127.0.0.0/8`, `[::1]` — local development/test servers).
pub(crate) fn require_secure_endpoint(url: &str) -> Result<(), AuthError> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        let host = if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
            &authority[1..=end]
        } else {
            authority.split(':').next().unwrap_or("")
        };
        let loopback = host == "localhost"
            || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if loopback {
            return Ok(());
        }
    }
    Err(AuthError::HttpRequest {
        url: url.to_string(),
        reason: "token endpoint must use https:// (plain http is only allowed for a loopback host)".to_string(),
    })
}

/// Parses a successful (2xx) token endpoint response body.
pub(crate) fn parse_token_response(context: &str, body: &str) -> Result<TokenResponse, AuthError> {
    serde_json::from_str(body)
        .map_err(|e| AuthError::InvalidTokenResponse { context: context.to_string(), reason: e.to_string() })
}

/// Best-effort parse of an error response body into `(error, error_description)`.
/// Falls back to `("unknown_error", Some(body))` if the body isn't the
/// expected JSON shape at all, so a malformed/non-JSON error body from a
/// misbehaving server still surfaces something useful.
pub(crate) fn parse_error_body(body: &str) -> (String, Option<String>) {
    match serde_json::from_str::<TokenErrorBody>(body) {
        Ok(err) => (err.error, err.error_description),
        Err(_) => ("unknown_error".to_string(), Some(body.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_always_allowed_and_http_only_for_loopback() {
        for ok in [
            "https://auth.example.com/token",
            "HTTPS://auth.example.com/token",
            "http://127.0.0.1:8080/token",
            "http://localhost/token",
            "http://[::1]:9000/token",
        ] {
            assert!(require_secure_endpoint(ok).is_ok(), "{ok} should be allowed");
        }
        for bad in [
            "http://auth.example.com/token",
            "http://127.0.0.1.evil.example/token",
            "http://user@auth.example.com/token",
            "ftp://auth.example.com/token",
            "auth.example.com/token",
        ] {
            assert!(require_secure_endpoint(bad).is_err(), "{bad} should be refused");
        }
    }
}
