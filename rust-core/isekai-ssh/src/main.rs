//! `isekai-ssh`: single static-binary `ssh(1)` `ProxyCommand` wrapper that
//! reuses isekai-terminal's isekai-helper QUIC connection resilience
//! (`ISEKAI_SSH_DESIGN.md`). Phase S-1: `connect` only.

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

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout purity (see connect.rs's module docs) is why this is pinned to
    // stderr explicitly rather than relying on env_logger's default target,
    // which callers should not have to trust blindly to stay stderr-only
    // across dependency upgrades.
    env_logger::Builder::from_default_env().target(env_logger::Target::Stderr).init();

    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Connect(args) => connect::run(args).await,
    }
}
