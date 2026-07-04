//! RFC 8628 OAuth 2.0 Device Authorization Grant client
//! (`ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー", フェーズ分割案 S-5). Used by
//! `isekai-ssh login` (`isekai-ssh/src/login.rs`) to obtain an initial
//! access/refresh token pair without embedding any OAuth client secret in
//! the CLI — device flow is a public-client flow (RFC 8628 §1). PKCE / an
//! in-app OAuth client are explicitly out of scope for this phase
//! (`ISEKAI_SSH_DESIGN.md` "含めないもの").
//!
//! The device-authorization/token endpoints and `client_id` are not
//! hardcoded here: the real Auth0 tenant URL isn't fixed yet
//! (`ISEKAI_SSH_DESIGN.md` "引き続き未決の項目"), so `isekai-ssh login`
//! exposes them as CLI flags (`--device-auth-endpoint`/`--token-endpoint`/
//! `--client-id`) that get threaded straight into `DeviceFlowConfig`.
//!
//! All requests are blocking (see `oauth`'s module docs for why); callers on
//! an async runtime (`isekai-ssh login`) should run `poll_for_token` inside
//! `tokio::task::spawn_blocking` since it sleeps the calling thread between
//! polls.

use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::oauth::{self, TokenResponse};
use crate::AuthError;

/// Where to send the two Device Authorization Grant requests, and which
/// public client is polling.
#[derive(Debug, Clone)]
pub struct DeviceFlowConfig {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
}

/// RFC 8628 §3.2 device authorization response.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_poll_interval")]
    pub interval: u64,
}

fn default_poll_interval() -> u64 {
    // RFC 8628 §3.2: "If no value is provided, clients MUST use 5 as the
    // default."
    5
}

impl DeviceAuthorization {
    /// The URI to show the user: prefers `verification_uri_complete`
    /// (RFC 8628 §3.3.1 — it embeds `user_code`, so the user doesn't have to
    /// type it in manually) and falls back to the plain `verification_uri`.
    pub fn display_uri(&self) -> &str {
        self.verification_uri_complete.as_deref().unwrap_or(&self.verification_uri)
    }
}

/// RFC 8628 §3.1: kicks off the flow by requesting a `device_code`/`user_code`
/// pair from `config.device_authorization_endpoint`.
pub fn request_device_authorization(config: &DeviceFlowConfig) -> Result<DeviceAuthorization, AuthError> {
    let (status, body) =
        oauth::post_form(&config.device_authorization_endpoint, &[("client_id", config.client_id.as_str())])?;

    if (200..300).contains(&status) {
        serde_json::from_str(&body).map_err(|e| AuthError::InvalidTokenResponse {
            context: "device_authorization".to_string(),
            reason: e.to_string(),
        })
    } else {
        let (error, description) = oauth::parse_error_body(&body);
        Err(AuthError::OAuthError { grant: "device_authorization", error, description })
    }
}

/// RFC 8628 §3.4/§3.5: polls `config.token_endpoint` at `authz.interval`
/// second intervals (growing by 5s on `slow_down`, RFC 8628 §3.5) until the
/// user completes the browser step, denies the request, the device code
/// expires, or an unrecoverable error is returned.
///
/// Blocking: sleeps the calling thread between polls (see module docs).
pub fn poll_for_token(config: &DeviceFlowConfig, authz: &DeviceAuthorization) -> Result<TokenResponse, AuthError> {
    let mut interval = Duration::from_secs(authz.interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(authz.expires_in);

    loop {
        thread::sleep(interval);
        if Instant::now() >= deadline {
            return Err(AuthError::DeviceFlowExpired);
        }

        let (status, body) = oauth::post_form(
            &config.token_endpoint,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &authz.device_code),
                ("client_id", &config.client_id),
            ],
        )?;

        if (200..300).contains(&status) {
            return oauth::parse_token_response("token", &body);
        }

        let (error, description) = oauth::parse_error_body(&body);
        match error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                // RFC 8628 §3.5: "the client's next request MUST be delayed
                // by the interval specified in the previous response plus
                // an additional 5 seconds."
                interval += Duration::from_secs(5);
                continue;
            }
            "access_denied" => return Err(AuthError::DeviceFlowDenied),
            "expired_token" => return Err(AuthError::DeviceFlowExpired),
            _ => return Err(AuthError::OAuthError { grant: "token", error, description }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_uri_prefers_verification_uri_complete() {
        let authz = DeviceAuthorization {
            device_code: "dc".to_string(),
            user_code: "ABCD-EFGH".to_string(),
            verification_uri: "https://example.com/device".to_string(),
            verification_uri_complete: Some("https://example.com/device?user_code=ABCD-EFGH".to_string()),
            expires_in: 600,
            interval: 5,
        };
        assert_eq!(authz.display_uri(), "https://example.com/device?user_code=ABCD-EFGH");
    }

    #[test]
    fn display_uri_falls_back_to_plain_verification_uri() {
        let authz = DeviceAuthorization {
            device_code: "dc".to_string(),
            user_code: "ABCD-EFGH".to_string(),
            verification_uri: "https://example.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
        };
        assert_eq!(authz.display_uri(), "https://example.com/device");
    }

    #[test]
    fn default_interval_is_five_seconds_per_rfc_8628() {
        let parsed: DeviceAuthorization = serde_json::from_str(
            r#"{"device_code":"dc","user_code":"ABCD","verification_uri":"https://example.com","expires_in":600}"#,
        )
        .unwrap();
        assert_eq!(parsed.interval, 5);
    }
}
