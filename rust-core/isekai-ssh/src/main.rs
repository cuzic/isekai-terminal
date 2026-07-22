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

/// Exit code a mux *client* returns when it detects the owner process died
/// mid-session (its `local-ipc-mux` connection dropped without a clean session
/// end). Deliberately distinct from `EXIT_OTHER_ERROR` (1) and from `ssh(1)`'s
/// own 255 ("connection lost / could not execute"), so this specific,
/// recoverable situation — "the shared owner went away; just reconnect" — is
/// distinguishable from both a generic error and a normal SSH connection loss.
/// 254 is otherwise unused by this codebase's exit conventions. See
/// `native/mux/client.rs`'s re-election model.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const EXIT_MUX_OWNER_LOST: u8 = 254;

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

/// Installs a process-wide panic hook that runs *in addition to* (not
/// instead of) the default stderr behavior, so a panic still shows on the
/// terminal exactly as before, but also gets recorded into whichever
/// `log_file.rs` sink is already active — the explicit `--isekai-log-file`
/// target if given, otherwise the always-on default verbose log
/// (`log_file::append_verbose_line`/`init_verbose`, itself a silent no-op
/// until `wrapper::run`/`native::connect::run` calls `init_verbose` early
/// on). Without this, a panic simply vanished once the terminal's own
/// scrollback was gone: `main()`'s `.join().unwrap_or_else(|panic|
/// std::panic::resume_unwind(panic))` only re-raises the payload for the
/// process's own exit handling, it never logs it anywhere. Must be
/// installed before the worker thread below is spawned — the hook is
/// process-wide, so one installation here also covers panics on that
/// thread. Fail-open like every other write in `log_file.rs`: appending
/// here is itself best-effort and must never panic in turn (a panic inside
/// a panic hook aborts the process immediately).
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let backtrace = std::backtrace::Backtrace::force_capture();
        let line = format!("PANIC: {info}\n{backtrace}");
        if log_file::is_enabled() {
            log_file::append_line(&line);
        } else {
            log_file::append_verbose_line(&line);
        }
    }));
}

fn main() {
    install_panic_hook();
    let exit_code = std::thread::Builder::new()
        .name("isekai-ssh-main".to_string())
        .stack_size(MAIN_WORKER_STACK_SIZE)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            let exit_code = runtime.block_on(run());
            // A dropped-but-still-pending `tokio::io::stdin()` read (the shell
            // I/O loop's stdin branch loses the race against the remote
            // channel closing — see `native/connect.rs::run_shell_io_loop`'s
            // module docs) keeps running on tokio's blocking pool even after
            // this future returns. The ordinary (implicit) `Drop` for
            // `Runtime` blocks until every outstanding blocking-pool task
            // finishes, which for a real terminal stdin means "until the user
            // presses another key" — exactly the hang this sidesteps.
            // `shutdown_background()` tears the runtime down without waiting,
            // matching `ssh(1)`'s own immediate-exit behavior.
            runtime.shutdown_background();
            exit_code
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
        // Windows never shells out to a real `ssh(1)` — the native path is a
        // from-scratch `russh`-based client that never spawns Win32-OpenSSH.
        // `native::mux::run` is the `ControlMaster`-equivalent dispatch: it
        // becomes the owner of (or a client to) the shared connection for this
        // resolved destination, falling back to the plain single-process
        // `native::connect::run` path when multiplexing isn't possible. Unix/
        // macOS keep the original `ssh(1)` ProxyCommand wrapper unchanged
        // (`native/mod.rs`'s module docs: this module is built and unit-tested
        // everywhere, but only ever *invoked* here).
        #[cfg(windows)]
        let result = native::mux::run(raw_args).await;
        #[cfg(not(windows))]
        let result = wrapper::run(raw_args).await;

        match result {
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
