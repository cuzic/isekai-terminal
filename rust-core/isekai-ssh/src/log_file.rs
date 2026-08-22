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
//!
//! The two process-wide targets ([`LOG_FILE`]/[`VERBOSE_LOG_FILE`]) are both
//! [`Sink`] instances: a single `OnceLock<Mutex<File>>`-backed open/append
//! implementation, since `init`/`append_line` and `init_verbose`/
//! `append_verbose_line` used to be two independent, near-identical copies
//! of that exact logic. One real drift this unification fixes: `init`
//! (`--isekai-log-file`) never created its target's parent directory or
//! restricted its permissions to `0o600` on Unix, while `init_verbose` (the
//! always-on default sink) already did both — folding both into
//! [`Sink::open`] makes `--isekai-log-file` do the same now, a small,
//! deliberate hardening riding along with the dedup rather than a
//! functional change anyone depends on the old gap for.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// A single process-wide log target: `open()` installs the backing file (at
/// most once — a second `open()` call is a caller bug, per [`init`]'s doc
/// comment, so its handle is simply dropped), `append_line`/`append_bytes`
/// write to it, and both silently no-op before `open()` succeeds or once
/// installed but subsequently unwritable (disk full, file removed out from
/// under the process, a poisoned lock from an earlier panic while holding
/// it) — logging must never be able to fail the actual command.
struct Sink(OnceLock<Mutex<File>>);

impl Sink {
    const fn new() -> Self {
        Sink(OnceLock::new())
    }

    fn is_enabled(&self) -> bool {
        self.0.get().is_some()
    }

    /// Opens (creating parent dirs as needed) `path` and installs it as this
    /// sink's target, always appending (never truncating the *file itself*
    /// — repeated invocations during one debugging session accumulate a
    /// single history rather than each overwriting the last) with `0o600`
    /// permissions on Unix. `truncate_over`, when given, first removes a
    /// pre-existing file already larger than that many bytes — the default
    /// verbose sink's bounded-growth safety net (see
    /// [`VERBOSE_LOG_MAX_BYTES`]); the explicit `--isekai-log-file` sink
    /// passes `None` and append-forevers by design.
    fn open(&self, path: &Path, truncate_over: Option<u64>) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(max_bytes) = truncate_over {
            if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > max_bytes {
                let _ = std::fs::remove_file(path);
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        // `OnceLock::set` failing (already initialized) would mean the
        // caller opened this sink twice — a caller bug, not a runtime
        // condition to handle gracefully — so the second file handle is
        // simply dropped.
        let _ = self.0.set(Mutex::new(file));
        Ok(())
    }

    /// Appends raw `bytes` verbatim (no line-ending massaging — callers that
    /// have whole lines already terminated should just include the `\n`).
    fn append_bytes(&self, bytes: &[u8]) {
        let Some(file) = self.0.get() else { return };
        let Ok(mut file) = file.lock() else { return };
        let _ = file.write_all(bytes);
        let _ = file.flush();
    }

    /// Appends one already-formatted line, prefixed with a UTC RFC 3339
    /// timestamp (ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-5) — a trailing `\n`
    /// is always added, whether or not `line` has one. Checks
    /// [`is_enabled`](Self::is_enabled) itself first so a disabled sink skips
    /// building the `String` buffer entirely, rather than relying solely on
    /// [`append_bytes`](Self::append_bytes)'s own (otherwise-sufficient)
    /// no-op check.
    ///
    /// UTC, not local time: `isekai_trust::now_rfc3339()` (already a
    /// dependency of this crate, reused here rather than adding a new one)
    /// only ever formats UTC — getting the local offset portably (this
    /// module runs on Linux/macOS/Windows) would need a real timezone
    /// dependency this codebase has deliberately avoided so far. Every line
    /// this module ever writes is diagnostic-only, never parsed back, so a
    /// reader mentally converting UTC once is a small cost against a real
    /// diagnostic gap: before this, nothing in either log file (this crate's
    /// own `--isekai-log-file`, or the always-on default verbose sink) could
    /// answer "when did this happen" at all, which stalled a real
    /// investigation (see that ADR's Q-1).
    fn append_line(&self, line: &str) {
        if !self.is_enabled() {
            return;
        }
        let timestamp = isekai_trust::now_rfc3339();
        let mut buf = String::with_capacity(timestamp.len() + 3 + line.len() + 1);
        buf.push('[');
        buf.push_str(&timestamp);
        buf.push_str("] ");
        buf.push_str(line);
        buf.push('\n');
        self.append_bytes(buf.as_bytes());
    }
}

static LOG_FILE: Sink = Sink::new();
static VERBOSE_LOG_FILE: Sink = Sink::new();

/// Truncated if larger than this when (re-)opened — a lightweight safety
/// net against unbounded growth now that this file is written by default
/// rather than only when a user explicitly opts into `--isekai-log-file`.
/// Not a real rotation scheme (matches `--isekai-log-file`'s own
/// append-forever behavior otherwise); just prevents an all-day flaky-WiFi
/// session from growing this file without bound.
const VERBOSE_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Opens the default verbose log at `path` and installs it as the
/// process-wide verbose-log target. Called at most once, from `run()`, only
/// when `--isekai-log-file` was *not* given (that flag's own [`init`] above
/// takes priority in [`log_line_verbose!`]). Failure here (permissions,
/// read-only filesystem, ...) is not fatal — `run()` simply proceeds
/// without verbose logging enabled, same "never block the connection over a
/// diagnostics nicety" philosophy as every other write in this module.
pub fn init_verbose(path: &Path) -> std::io::Result<()> {
    VERBOSE_LOG_FILE.open(path, Some(VERBOSE_LOG_MAX_BYTES))
}

/// Appends one already-formatted line to the default verbose log, silently
/// doing nothing if [`init_verbose`] was never called or failed (same
/// fail-open policy as [`append_line`]).
pub fn append_verbose_line(line: &str) {
    VERBOSE_LOG_FILE.append_line(line);
}

/// Opens (creating if needed, always appending) `path` and installs it as
/// the process-wide log file. Must be called at most once; `run()` only
/// calls this when `--isekai-log-file` was actually given.
pub fn init(path: &Path) -> std::io::Result<()> {
    LOG_FILE.open(path, None)
}

pub fn is_enabled() -> bool {
    LOG_FILE.is_enabled()
}

/// Appends one already-formatted line — used by [`log_line!`].
pub fn append_line(line: &str) {
    LOG_FILE.append_line(line);
}

/// Writes `line` to [`LOG_FILE`] if `--isekai-log-file` is active, otherwise
/// to `fallback` — the branching [`log_line!`]/[`log_line_verbose!`] used to
/// each re-implement per macro arm (four near-identical copies total: two
/// macros × the empty-args/format-args arms each needs). `fallback` is what
/// keeps the two macros meaningfully different — `log_line!`'s prints to
/// this process's own stderr, `log_line_verbose!`'s writes to the always-on
/// default verbose sink — not just "which file," so they remain two
/// macros, each now a thin wrapper around this one function instead of
/// hand-rolling the `is_enabled` branch itself.
pub(crate) fn dispatch(line: &str, fallback: impl FnOnce(&str)) {
    if is_enabled() {
        append_line(line);
    } else {
        fallback(line);
    }
}

/// Drop-in replacement for `eprintln!` used throughout `wrapper.rs`: when no
/// log file is configured, behaves exactly like `eprintln!` (prints to
/// stderr, nothing else). When `--isekai-log-file` *is* active, the line
/// goes to the log file **instead of** stderr — nothing from this macro
/// reaches the terminal while a log file is configured.
macro_rules! log_line {
    () => {{
        $crate::log_file::dispatch("", |_line| eprintln!());
    }};
    ($($arg:tt)*) => {{
        $crate::log_file::dispatch(&format!($($arg)*), |line| eprintln!("{line}"));
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
        $crate::log_file::dispatch("", |line| $crate::log_file::append_verbose_line(line));
    }};
    ($($arg:tt)*) => {{
        $crate::log_file::dispatch(&format!($($arg)*), |line| $crate::log_file::append_verbose_line(line));
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
        LOG_FILE.append_bytes(&buf[..n]);
    }
}
