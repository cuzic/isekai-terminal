//! Real-ELF stand-in for `isekai-pipe serve`, used only by
//! `tests/openssh_e2e.rs`'s helper-reuse tests. Those tests need
//! `/proc/<pid>/exe` to resolve back to the exact uploaded file — a
//! shell-script stand-in (like every other test in that file uses) would
//! have its `exe` resolve to its `#!/bin/sh` interpreter instead, since
//! that's what the kernel actually execs for a shebang script, which would
//! make `OpenSshBackend::install_and_launch`'s PID-reuse guard always see a
//! mismatch and defeat the very reuse behavior under test. Not shipped
//! anywhere; see the `[[bin]]` entry in `Cargo.toml`.
//!
//! Configuration travels through files under `$HOME` (set per-invocation by
//! the mock ssh server to each test's own scratch tempdir,
//! `tests/openssh_e2e.rs`'s `spawn_fake_ssh_server`) rather than environment
//! variables, so parallel tests never share mutable global state.

use std::io::Write as _;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("serve") {
        std::process::exit(1);
    }

    let home = std::env::var("HOME").unwrap_or_default();

    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(format!("{home}/fake-pipe-invocations.log"))
    {
        let _ = writeln!(f, "invoked");
    }

    if let Ok(handshake) = std::fs::read_to_string(format!("{home}/fake-pipe-handshake.json")) {
        // stdout is fully buffered (not line-buffered) once redirected to a
        // file, unlike a terminal — without an explicit flush here, the
        // poll loop in `OpenSshBackend::install_and_launch`'s script would
        // see an empty `$tmpdir/handshake` for the entire duration of the
        // `sleep` below and time out, since nothing forces the buffer out
        // before this process exits.
        print!("{handshake}");
        std::io::stdout().flush().ok();
    }

    // Stays alive long enough for a test to observe it via `kill -0`/
    // `/proc/<pid>/exe`, then exits on its own rather than lingering as an
    // orphaned process on the test machine indefinitely.
    std::thread::sleep(std::time::Duration::from_secs(20));
}
