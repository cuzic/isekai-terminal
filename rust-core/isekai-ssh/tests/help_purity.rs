//! Acceptance test for `ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-1's release-build
//! requirement: a plain `cargo build --release -p isekai-ssh` (no
//! `--features` at all — the `dev-insecure` feature is not a default
//! feature, see `Cargo.toml`) must produce a binary whose
//! `connect --help` output never mentions the `--dev-insecure-*` bypass
//! flags. Those flags exist only to unblock `connect_e2e.rs` before the real
//! trust store (S-2/S-3) is wired up, and must never be visible — let alone
//! usable — in anything actually shipped.
//!
//! This deliberately invokes a *fresh* `cargo build --release` rather than
//! relying on `CARGO_BIN_EXE_isekai-ssh` (which would reflect whatever
//! feature set *this test itself* happens to be compiled with, not a
//! from-scratch default release build) — the whole point is to check the
//! artifact an end user actually gets from `cargo install`/a release
//! pipeline.

use std::path::PathBuf;
use std::process::Command;

fn target_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `.../rust-core/isekai-ssh`; the shared
    // workspace target dir is one level up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().expect("isekai-ssh has a parent workspace dir").join("target")
}

#[test]
fn release_build_connect_help_never_mentions_dev_insecure_flags() {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", "isekai-ssh"])
        .status()
        .expect("failed to invoke `cargo build --release -p isekai-ssh`");
    assert!(status.success(), "`cargo build --release -p isekai-ssh` (no --features) failed");

    let bin = target_dir().join("release").join("isekai-ssh");
    assert!(bin.exists(), "expected release binary at {bin:?}");

    let output =
        Command::new(&bin).arg("connect").arg("--help").output().expect("failed to run `isekai-ssh connect --help`");
    assert!(output.status.success(), "`isekai-ssh connect --help` exited non-zero: {:?}", output.status);

    let help_text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));

    // Sanity check first: prove the help text we captured is the real thing
    // (a documented, always-present flag), not an empty/garbled capture that
    // would make the negative assertions below vacuously true.
    assert!(help_text.contains("--via"), "expected the always-present --via flag in --help output: {help_text}");

    for marker in ["dev-insecure", "dev_insecure"] {
        assert!(
            !help_text.to_lowercase().contains(marker),
            "release build's `connect --help` must never mention `{marker}`, but it did:\n{help_text}"
        );
    }
}
