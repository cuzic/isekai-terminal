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
//!
//! A second, independent channel ([`append_verbose_line`]/[`log_line_verbose!`])
//! backs the *default* (no flag needed) quiet behavior: verbose bootstrap/
//! diagnostic detail always goes to `isekai_pipe_core::default_log_file()`
//! instead of the terminal, without touching `is_enabled()` — which also
//! gates whether `wrapper.rs` pipes `ssh(1)`'s child stderr (see
//! `run_ssh_once`). Conflating the two would route `resume_loop.rs`'s
//! human-facing reconnect status lines into a log file by default too,
//! defeating the point (found during design review before implementing —
//! see the plan for this change).

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static VERBOSE_LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Truncated if larger than this when (re-)opened — a lightweight safety
/// net against unbounded growth now that this file is written by default
/// rather than only when a user explicitly opts into `--isekai-log-file`.
/// Not a real rotation scheme (matches `--isekai-log-file`'s own
/// append-forever behavior otherwise); just prevents an all-day flaky-WiFi
/// session from growing this file without bound.
const VERBOSE_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Opens (creating parent dirs + the file as needed) the default verbose
/// log at `path` and installs it as the process-wide verbose-log target.
/// Called at most once, from `run()`, only when `--isekai-log-file` was
/// *not* given (that flag's own `init` above takes priority in
/// `log_line_verbose!`). Truncates first if the existing file already
/// exceeds [`VERBOSE_LOG_MAX_BYTES`]. Failure here (permissions, read-only
/// filesystem, ...) is not fatal — `run()` simply proceeds without verbose
/// logging enabled, same "never block the connection over a diagnostics
/// nicety" philosophy as `append_bytes`.
pub fn init_verbose(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > VERBOSE_LOG_MAX_BYTES {
        let _ = std::fs::remove_file(path);
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let _ = VERBOSE_LOG_FILE.set(Mutex::new(file));
    Ok(())
}

/// Appends one already-formatted line to the default verbose log, silently
/// doing nothing if `init_verbose` was never called or failed (same
/// fail-open policy as [`append_line`]/[`append_bytes`]).
pub fn append_verbose_line(line: &str) {
    let Some(file) = VERBOSE_LOG_FILE.get() else { return };
    let Ok(mut file) = file.lock() else { return };
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    let _ = file.write_all(buf.as_bytes());
    let _ = file.flush();
}

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

/// Verbose/detail counterpart to [`log_line!`] for bootstrap-progress-style
/// messages that don't need to be on screen by default. When
/// `--isekai-log-file` is active, behaves exactly like `log_line!` (goes
/// into that one unified file, preserving its "everything in one place"
/// contract). Otherwise, goes quietly to the always-on default verbose log
/// (`init_verbose`/`append_verbose_line`) instead of the terminal —
/// nothing from this macro reaches the screen in the default (no flags)
/// case.
macro_rules! log_line_verbose {
    () => {{
        if $crate::log_file::is_enabled() {
            $crate::log_file::append_line("");
        } else {
            $crate::log_file::append_verbose_line("");
        }
    }};
    ($($arg:tt)*) => {{
        if $crate::log_file::is_enabled() {
            $crate::log_file::append_line(&format!($($arg)*));
        } else {
            $crate::log_file::append_verbose_line(&format!($($arg)*));
        }
    }};
}
pub(crate) use log_line_verbose;

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
