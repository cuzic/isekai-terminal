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
#[derive(Debug, Clone, Deserialize)]
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
pub(crate) fn post_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError> {
    let mut response = ureq::post(url)
        .config()
        .http_status_as_error(false)
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
