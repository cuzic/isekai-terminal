//! OAuth2 `refresh_token` grant (RFC 6749 §6), used by
//! `FileTokenProvider::get_relay_jwt` (`file_provider.rs`) to transparently
//! refresh a near-expiry/expired access token
//! (`archive/ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー": "`connect` 実行中のトークン
//! 失効は裏で自動リフレッシュを試みる"). Wiring this into `isekai-ssh connect`
//! itself is out of scope for phase S-5 (`connect.rs` is unchanged) — this
//! module only provides the primitive `get_relay_jwt` calls internally.
//!
//! Shares its wire format (`oauth::TokenResponse`) and low-level HTTP POST
//! plumbing (`oauth::post_form`) with `device_flow`'s token-endpoint polling
//! — both grants hit the same token endpoint and get back the same
//! `{access_token, refresh_token?, expires_in?}` shape on success and the
//! same `{error, error_description?}` shape on failure (RFC 6749 §5).

use crate::oauth::{self, TokenResponse};
use crate::AuthError;

/// RFC 6749 §6: exchanges `refresh_token` for a fresh access token (and,
/// often, a rotated refresh token — see `TokenResponse::refresh_token`).
/// `client_id` is included when present since public clients (device flow
/// has no client secret) still authenticate this way per RFC 6749 §2.3.1.
pub fn refresh_access_token(
    token_endpoint: &str,
    client_id: Option<&str>,
    refresh_token: &str,
) -> Result<TokenResponse, AuthError> {
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "refresh_token"), ("refresh_token", refresh_token)];
    if let Some(client_id) = client_id {
        form.push(("client_id", client_id));
    }

    let (status, body) = oauth::post_form(token_endpoint, &form)?;
    if (200..300).contains(&status) {
        oauth::parse_token_response("refresh_token", &body)
    } else {
        let (error, description) = oauth::parse_error_body(&body);
        Err(AuthError::OAuthError { grant: "refresh_token", error, description })
    }
}
