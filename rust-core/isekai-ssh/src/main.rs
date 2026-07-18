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
mod doctor;
mod helper_download;
mod init;
mod log_file;
mod login;
mod native;
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

/// Larger than the OS-default *process* main thread stack, which is fixed
/// at link time and cannot be grown at runtime -- notably 1 MiB on Windows
/// (vs. ~8 MiB typical on Linux/macOS). `tokio::main`'s generated
/// `block_on` runs the top-level future's synchronous work (e.g. the QUIC
/// handshake in `quicmux`/`noq`, which is deep and un-inlined in debug
/// builds) directly on the calling thread, so a small main-thread stack
/// overflows there before this fix -- observed in practice as
/// `thread 'main' has overflowed its stack` right after
/// `quicmux::noq_backend` starts connecting on a debug build on Windows.
/// Running the whole async body on a freshly spawned thread with an
/// explicit stack size sidesteps the platform-fixed main-thread limit; the
/// size chosen here is generous headroom rather than a tight bound.
const MAIN_WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() {
    let exit_code = std::thread::Builder::new()
        .name("isekai-ssh-main".to_string())
        .stack_size(MAIN_WORKER_STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(run())
        })
        .expect("failed to spawn isekai-ssh main worker thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    std::process::exit(exit_code as i32);
}

async fn run() -> u8 {
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
                // `wrapper::run` already opened `--isekai-log-file` (if given)
                // before this point could be reached on any path that also
                // parsed it, so a top-level wrapper-mode failure is exactly
                // the kind of message `log_file.rs` exists to capture too.
                log_file::log_line!("{err:?}");
                EXIT_OTHER_ERROR
            }
        }
    } else if matches!(raw_args.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        // `--help`/`-h`/`help` bypass `Cli::parse()`'s default rendering
        // (which only lists subcommand one-liners) so every subcommand's
        // own options show up in a single `isekai-ssh --help` — see
        // `cli::print_full_help`'s docs.
        cli::print_full_help();
        0
    } else {
        let cli = cli::Cli::parse();
        let result = match cli.command {
            cli::Command::Init(args) => init::run(*args).await,
            cli::Command::Login(args) => login::run(args).await,
            cli::Command::Logout => login::run_logout().await,
            cli::Command::Doctor(args) => doctor::run(args).await,
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

    exit_code
}
