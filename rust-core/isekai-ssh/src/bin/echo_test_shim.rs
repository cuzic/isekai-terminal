//! Test fixture standing in for `isekai-pipe connect --stdio`
//! (`native::child_stdio`'s tests): first prints one `NAME=value-or-(unset)`
//! line for each of `ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR` (verifying
//! a caller like `spawn_isekai_pipe_connect` actually set the env vars it
//! claims to, without needing a real `isekai-pipe` binary or mutating this
//! test process's own environment — no `std::env::set_var` anywhere, since
//! that's process-global and races against concurrently-running tests),
//! then streams stdin back to stdout byte-for-byte, incrementally (not
//! buffered-until-EOF, since the test writer keeps its side of the pipe
//! open and expects to read an immediate echo back).
//!
//! A genuine compiled `.exe` rather than a shell script, matching
//! `ssh_test_shim.rs`'s precedent, so this also works unmodified on Windows
//! (no reliance on `cat`(1) being on `PATH`, which isn't guaranteed there).

use std::io::{self, Read, Write};

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for name in ["ISEKAI_INTENT_ID", "ISEKAI_PIPE_RUNTIME_DIR"] {
        let value = std::env::var(name).unwrap_or_else(|_| "(unset)".to_string());
        writeln!(out, "{name}={value}").expect("write to stdout");
    }
    out.flush().expect("flush stdout");

    let stdin = io::stdin();
    let mut locked_stdin = stdin.lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match locked_stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
            break;
        }
    }
}
