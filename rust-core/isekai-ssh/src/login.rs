//! `isekai-ssh login`/`logout` (`ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー",
//! フェーズ分割案 S-5).
//!
//! `login` runs an RFC 8628 Device Authorization Grant, implemented in
//! `isekai_auth::device_flow` and orchestrated here: request a
//! `device_code`/`user_code` pair, show the user where to go and what code
//! to enter, poll the token endpoint until they finish (or deny, or the code
//! expires), then save the resulting access/refresh token pair via
//! `isekai_auth::FileTokenProvider` (`~/.config/isekai-ssh/token.json`).
//! `logout` just deletes that file.
//!
//! Unlike `connect.rs`, stdout purity is not a constraint here — `login`/
//! `logout` are never invoked as `ssh`'s `ProxyCommand` (same reasoning as
//! `init.rs`) — so progress, the verification URL, and the user code are
//! printed directly to stdout.
//!
//! The three OAuth endpoints/`client_id` are required CLI flags
//! (`cli::LoginArgs`) rather than hardcoded: the real Auth0 tenant URL isn't
//! fixed yet (`ISEKAI_SSH_DESIGN.md` "引き続き未決の項目"), so hardcoding a
//! placeholder here would just have to be replaced later anyway.
//!
//! `isekai_auth::device_flow`'s HTTP calls are blocking (`ureq`, see that
//! module's docs for why); this module runs them inside
//! `tokio::task::spawn_blocking` so `login` doesn't tie up a tokio worker
//! thread for the whole (multi-second, RFC 8628 §3.5) polling loop, and so a
//! `#[tokio::test]` alongside an in-process mock HTTP server on the same
//! runtime doesn't deadlock.

use anyhow::{Context, Result};
use isekai_auth::device_flow::{self, DeviceAuthorization, DeviceFlowConfig};
use isekai_auth::{FileTokenProvider, TokenResponse, TokenSet};

use crate::cli::LoginArgs;

pub async fn run(args: LoginArgs) -> Result<()> {
    let config = DeviceFlowConfig {
        device_authorization_endpoint: args.device_auth_endpoint.clone(),
        token_endpoint: args.token_endpoint.clone(),
        client_id: args.client_id.clone(),
    };

    let authz: DeviceAuthorization = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || device_flow::request_device_authorization(&config))
            .await
            .context("isekai-ssh: device authorization request task panicked")??
    };

    println!("To finish logging in, open:");
    println!();
    println!("    {}", authz.display_uri());
    println!();
    if authz.verification_uri_complete.is_none() {
        println!("and enter the code: {}", authz.user_code);
        println!();
    }
    println!("Waiting for confirmation...");

    let token: TokenResponse = {
        let config = config.clone();
        let authz = authz.clone();
        tokio::task::spawn_blocking(move || device_flow::poll_for_token(&config, &authz))
            .await
            .context("isekai-ssh: token polling task panicked")??
    };

    let token_set =
        TokenSet::from_token_response(token, args.token_endpoint.clone(), Some(args.client_id.clone()), None);

    let provider = FileTokenProvider::from_default_path()
        .context("isekai-ssh: could not determine the token file path (is $HOME set?)")?;
    provider.save_token_set(&token_set).context("isekai-ssh: failed to save the obtained token")?;

    let token_path = isekai_auth::default_token_path()
        .context("isekai-ssh: could not determine the token file path (is $HOME set?)")?;
    println!("Logged in — token saved to {}", token_path.display());
    Ok(())
}

pub async fn run_logout() -> Result<()> {
    let path = isekai_auth::default_token_path()
        .context("isekai-ssh: could not determine the token file path (is $HOME set?)")?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Logged out — removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Already logged out ({} does not exist)", path.display());
        }
        Err(e) => return Err(e).with_context(|| format!("isekai-ssh: failed to remove {}", path.display())),
    }
    Ok(())
}
