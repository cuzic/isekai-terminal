//! `isekai-ssh`: single static-binary `ssh(1)` `ProxyCommand` wrapper that
//! reuses isekai-terminal's isekai-helper QUIC connection resilience
//! (`ISEKAI_SSH_DESIGN.md`). Phase S-2: `connect` backed by the trust store.

// The `dev-insecure` feature bypasses the (not-yet-implemented) trust store
// so this phase's early end-to-end test can run before S-2 lands. It must
// never ship in a release binary: fail the build outright if anyone passes
// `--release --features dev-insecure` (or sets `debug-assertions = true` in
// a release-like profile while the feature is on), rather than relying on
// `--help` output alone to catch the mistake.
#[cfg(all(not(debug_assertions), feature = "dev-insecure"))]
compile_error!(
    "isekai-ssh: the `dev-insecure` feature must never be combined with a build that has \
     debug_assertions disabled (i.e. a release build). It bypasses the trust-store lookup that is \
     the only thing standing between `connect` and an attacker-supplied isekai-helper endpoint once \
     the trust store (ISEKAI_SSH_DESIGN.md S-2) lands, and must never reach a distributed binary."
);

mod cli;
mod connect;
mod init;
mod login;
mod resume;

use clap::Parser;

/// Exit code for "the target host has no trust store entry"
/// (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-2 "exit codeの分類": at minimum,
/// distinguish this from every other failure). `connect::run` signals this
/// case via the `connect::TrustNotInitialized` marker error.
const EXIT_TRUST_NOT_INITIALIZED: u8 = 10;
/// Every other `connect` failure (invalid arguments, relay/handshake
/// failure, I/O errors, ...).
const EXIT_OTHER_ERROR: u8 = 1;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // stdout purity (see connect.rs's module docs) is why this is pinned to
    // stderr explicitly rather than relying on env_logger's default target,
    // which callers should not have to trust blindly to stay stderr-only
    // across dependency upgrades.
    env_logger::Builder::from_default_env().target(env_logger::Target::Stderr).init();

    let cli = cli::Cli::parse();
    let result = match cli.command {
        cli::Command::Connect(args) => connect::run(args).await,
        cli::Command::Init(args) => init::run(args).await,
        cli::Command::Login(args) => login::run(args).await,
        cli::Command::Logout => login::run_logout().await,
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // stdout purity: errors are only ever printed to stderr, never
            // stdout (see connect.rs's module docs; `ssh`'s ProxyCommand
            // treats our stdout as raw SSH bytes).
            eprintln!("{err:?}");
            let is_trust_not_initialized =
                err.chain().any(|cause| cause.downcast_ref::<connect::TrustNotInitialized>().is_some());
            std::process::ExitCode::from(if is_trust_not_initialized {
                EXIT_TRUST_NOT_INITIALIZED
            } else {
                EXIT_OTHER_ERROR
            })
        }
    }
}
