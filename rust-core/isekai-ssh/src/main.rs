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
mod wrapper;

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
async fn main() {
    // stdout purity (see connect.rs's module docs) is why this is pinned to
    // stderr explicitly rather than relying on env_logger's default target,
    // which callers should not have to trust blindly to stay stderr-only
    // across dependency upgrades.
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let exit_code: u8 = if wrapper::should_run_wrapper(&raw_args) {
        match wrapper::run(raw_args).await {
            Ok(code) => code,
            Err(err) => {
                eprintln!("{err:?}");
                EXIT_OTHER_ERROR
            }
        }
    } else {
        let cli = cli::Cli::parse();
        let result = match cli.command {
            cli::Command::Connect(args) => connect::run(args).await,
            cli::Command::Init(args) => init::run(args).await,
            cli::Command::Login(args) => login::run(args).await,
            cli::Command::Logout => login::run_logout().await,
        };

        match result {
            Ok(()) => 0,
            Err(err) => {
                // stdout purity: errors are only ever printed to stderr, never
                // stdout (see connect.rs's module docs; `ssh`'s ProxyCommand
                // treats our stdout as raw SSH bytes).
                eprintln!("{err:?}");
                let is_trust_not_initialized = err.chain().any(|cause| {
                    cause
                        .downcast_ref::<connect::TrustNotInitialized>()
                        .is_some()
                });
                if is_trust_not_initialized {
                    EXIT_TRUST_NOT_INITIALIZED
                } else {
                    EXIT_OTHER_ERROR
                }
            }
        }
    };

    // `std::process::exit` — not `return`/`ExitCode` — deliberately, on every
    // path (`ISEKAI_SSH_DESIGN.md` Phase S-4d): `connect`'s stdin pump
    // (`connect.rs::pump_c2h`) reads from `tokio::io::stdin()`, which
    // dispatches each read to a background OS thread that Tokio's own docs
    // say is "not currently cancelled" on runtime shutdown and can leave the
    // process "hang ... indefinitely" if that thread is blocked in a read
    // call when the runtime is dropped. That is exactly `pump_c2h`'s steady
    // state (blocked waiting for the next keystroke) at the moment
    // `run_relay_resumable` gives up after exceeding the resume window: `ssh`
    // (the ProxyCommand parent) has no reason to have closed its write end of
    // this pipe yet, since from its perspective the session is still alive.
    // Letting `main` return normally here would drop the `#[tokio::main]`
    // runtime and block on that orphaned thread — silently reintroducing the
    // exact indefinite hang this phase exists to prevent, on every host where
    // `ServerAliveInterval 0` (no keepalive) leaves `ssh` itself with no
    // timeout of its own to eventually close that pipe
    // (`ISEKAI_SSH_DESIGN.md`'s "制約: sshの生存確認とのレース" note on this
    // exact configuration). `std::process::exit` terminates immediately
    // without waiting for any thread, orphaned or not. Flushing stdout/stderr
    // first ensures the `eprintln!` above (and anything `connect::run`'s
    // give-up path already flushed via `tokio`'s async `stdout.shutdown()`)
    // is not lost — `process::exit` skips destructors, not already-completed
    // writes, but a belt-and-suspenders flush costs nothing here.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(exit_code as i32);
}
