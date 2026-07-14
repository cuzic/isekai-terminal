//! Windows-only e2e test fixture: a tiny compiled passthrough standing in
//! for `ssh(1)`, injecting `-F <config>` ahead of whatever args it's given
//! before forwarding to the real `ssh`.
//!
//! Exists because a `.cmd`/`.bat` batch-file shim (the first thing tried
//! here, see `tests/wrapper_auto_bootstrap_e2e.rs` git history) can't
//! reliably carry `ssh(1)` remote-command arguments containing embedded
//! newlines: `std::process::Command`'s Windows batch-file argument-safety
//! validation (added for CVE-2024-24576/"BatBadBut") rejects any argument
//! containing `\r`/`\n` outright with `InvalidInput: "batch file arguments
//! are invalid"` — and `isekai-bootstrap`'s real deploy step's remote
//! command is exactly such a multi-line string (confirmed via a real
//! `test-windows` CI failure). A genuine compiled `.exe` sidesteps this
//! entirely: it's never treated as a batch file, so none of that special
//! casing applies, and ordinary Win32 argv passing handles embedded
//! newlines within a single argument just fine. It also sidesteps the
//! separate problem a bare `.cmd`/shebang-script shim had before this
//! (native `CreateProcessW` doesn't interpret POSIX shebangs, and a bare
//! `Command::new("ssh")` only implicitly resolves `.exe` on Windows) since
//! callers already point `--isekai-ssh-path` at this binary's full path
//! directly.
//!
//! Configured via two env vars (set by the spawning test, not CLI flags —
//! keeps `main` trivial and leaves this binary's own argv free to relay
//! `ssh(1)`'s real arguments verbatim, exactly as `isekai-ssh`'s internal
//! `Command::new(&plan.openssh_path).args(&plan.ssh_args)`-style call sites
//! invoke it):
//! - `ISEKAI_SSH_TEST_SHIM_REAL_SSH`: path to the real `ssh(1)`/`ssh.exe`.
//! - `ISEKAI_SSH_TEST_SHIM_CONFIG`: path to the throwaway `-F` config file.
//! Both are inherited from `isekai-ssh`'s own process environment (the test
//! sets them when spawning `isekai-ssh`, which in turn spawns this binary
//! without clearing its environment) — the same inheritance chain the
//! now-abandoned `.cmd` shim relied on for `%PATH%`.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let real_ssh = std::env::var_os("ISEKAI_SSH_TEST_SHIM_REAL_SSH")
        .expect("ISEKAI_SSH_TEST_SHIM_REAL_SSH must be set (see module docs)");
    let config = std::env::var_os("ISEKAI_SSH_TEST_SHIM_CONFIG")
        .expect("ISEKAI_SSH_TEST_SHIM_CONFIG must be set (see module docs)");

    let status = Command::new(real_ssh)
        .arg("-F")
        .arg(config)
        .args(std::env::args_os().skip(1))
        .status()
        .expect("ssh_test_shim: failed to exec the real ssh");

    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::FAILURE,
    }
}
