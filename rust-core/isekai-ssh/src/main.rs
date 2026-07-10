//! `isekai-ssh`: single static-binary `ssh(1)` `ProxyCommand` wrapper that
//! reuses isekai-terminal's isekai-helper QUIC connection resilience
//! (`archive/ISEKAI_SSH_DESIGN.md`).
//!
//! The day-to-day entry point is the non-subcommand wrapper mode
//! (`isekai-ssh <destination>`, `wrapper.rs`): it resolves `~/.ssh/config`
//! and `#@isekai` directives, then execs the real `ssh` with an injected
//! `ProxyCommand isekai-pipe connect ...` (`archive/ISEKAI_PIPE_MIGRATION.md` P4).
//! `init`/`login`/`logout` remain as the interactive subcommands that
//! populate/manage the trust store the wrapper reads from.

mod cli;
mod ctl_forward;
mod helper_download;
mod init;
mod login;
mod wrapper;

/// Serializes tests (across `init.rs`/`wrapper.rs`) that mutate the
/// process-global `$HOME` env var to point at a throwaway fixture
/// directory. `cargo test` runs `#[test]` functions on multiple threads
/// within the same process by default, and `std::env::set_var` has no
/// thread-local scoping — without this, one test's `$HOME` mutation can be
/// clobbered mid-flight by a concurrently-running test in a different
/// module, causing spurious "profile not found" failures that have nothing
/// to do with either test's actual logic.
#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use clap::Parser;

const EXIT_OTHER_ERROR: u8 = 1;

#[tokio::main]
async fn main() {
    // stdout purity (see wrapper.rs's module docs) is why this is pinned to
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
            cli::Command::Init(args) => init::run(*args).await,
            cli::Command::Login(args) => login::run(args).await,
            cli::Command::Logout => login::run_logout().await,
        };

        match result {
            Ok(()) => 0,
            Err(err) => {
                // stdout purity: errors are only ever printed to stderr,
                // never stdout.
                eprintln!("{err:?}");
                EXIT_OTHER_ERROR
            }
        }
    };

    std::process::exit(exit_code as i32);
}
