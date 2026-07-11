//! `--isekai-log-file <PATH>`: opt-in, redirects every byte of diagnostic
//! output for this invocation into a plain file *instead of* the terminal —
//! so a live debugging session survives past the terminal's own scrollback
//! and isn't tangled up with the interactive SSH session's own stdout, and
//! the terminal itself shows only that interactive session, nothing else.
//!
//! Two distinct sources feed the same file, neither reaching the terminal
//! while this is active:
//! - `isekai-ssh`'s own status messages (`wrapper.rs`'s `eprintln!` calls,
//!   converted to [`log_line!`]) — bootstrap/re-deploy progress, stale-trust
//!   notices, etc.
//! - `ssh(1)`'s own stderr, which is also where its `ProxyCommand`
//!   grandchild (`isekai-pipe connect`, `env_logger`-based) ends up writing
//!   — captured by `wrapper.rs::run_ssh_once` piping (rather than
//!   inheriting) just the child's stderr and relaying it through
//!   [`redirect_child_stderr`], deliberately leaving stdin/stdout
//!   `Stdio::inherit()`ed untouched (piping *those* would break `ssh`'s own
//!   `isatty()`-based PTY/interactive-terminal behavior — this module never
//!   touches them).
//!
//! Global, process-wide, set at most once (`init`, from `run()` before
//! anything else can log) — simpler than threading a handle through every
//! call site that currently just does `eprintln!`, and there is exactly one
//! `isekai-ssh` process per invocation, so nothing here needs to be
//! per-connection scoped.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Opens (creating if needed, always appending — repeated invocations
/// during one debugging session accumulate a single history rather than
/// each overwriting the last) `path` and installs it as the process-wide
/// log file. Must be called at most once; `run()` only calls this when
/// `--isekai-log-file` was actually given.
pub fn init(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    // `OnceLock::set` failing (already initialized) would mean `run()`
    // called this twice — a caller bug, not a runtime condition to handle
    // gracefully — so the second file handle is simply dropped.
    let _ = LOG_FILE.set(Mutex::new(file));
    Ok(())
}

pub fn is_enabled() -> bool {
    LOG_FILE.get().is_some()
}

/// Appends raw `bytes` verbatim (no line-ending massaging — callers that
/// have whole lines already terminated should just include the `\n`).
/// Silently drops the write on I/O failure (e.g. disk full, file removed
/// out from under us) — logging must never be able to fail the actual
/// command. A poisoned lock (a prior panic while holding it) is likewise
/// treated as "logging unavailable" rather than propagating the panic.
fn append_bytes(bytes: &[u8]) {
    let Some(file) = LOG_FILE.get() else { return };
    let Ok(mut file) = file.lock() else { return };
    let _ = file.write_all(bytes);
    let _ = file.flush();
}

/// Appends one already-formatted line (a trailing `\n` is always added,
/// whether or not `line` has one) — used by [`log_line!`].
pub fn append_line(line: &str) {
    if !is_enabled() {
        return;
    }
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    append_bytes(buf.as_bytes());
}

/// Drop-in replacement for `eprintln!` used throughout `wrapper.rs`: when no
/// log file is configured, behaves exactly like `eprintln!` (prints to
/// stderr, nothing else). When `--isekai-log-file` *is* active, the line
/// goes to the log file **instead of** stderr — nothing from this macro
/// reaches the terminal while a log file is configured.
macro_rules! log_line {
    () => {{
        if $crate::log_file::is_enabled() {
            $crate::log_file::append_line("");
        } else {
            eprintln!();
        }
    }};
    ($($arg:tt)*) => {{
        if $crate::log_file::is_enabled() {
            $crate::log_file::append_line(&format!($($arg)*));
        } else {
            eprintln!($($arg)*);
        }
    }};
}
pub(crate) use log_line;

/// Relays `child_stderr` into the log file **instead of** this process's
/// own stderr, until the child closes its stderr (normally, on exit) — the
/// terminal shows none of it while `--isekai-log-file` is active.
/// Deliberately only ever applied to `ssh(1)`'s *stderr* — see this
/// module's docs for why stdin/stdout must stay untouched (and therefore
/// still show the interactive SSH session on the terminal as normal).
pub async fn redirect_child_stderr(mut child_stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncReadExt as _;
    let mut buf = [0u8; 8192];
    loop {
        let n = match child_stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        append_bytes(&buf[..n]);
    }
}
