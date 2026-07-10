//! Real-`sshd` verification of `ctl_forward.rs`'s (`ISEKAI_PIPE_DESIGN.md`
//! §8 Epic M, `#@isekai ctl-socket yes`) remote-command wrapping: `export
//! ISEKAI_CTL_SOCK=<path>; <rest>` — needed because `-o SetEnv=...`
//! requires a matching `AcceptEnv`/`SetEnv` entry in the remote
//! `sshd_config` most users don't control — actually delivers the env var
//! to whatever `<rest>` execs, when `sshd` runs it via `$SHELL -c "..."`
//! (what happens whenever `ssh(1)` is given an explicit remote command,
//! which this feature always supplies — see
//! `ctl_forward::should_attempt_ctl_forward`).
//!
//! **Not covered here** (see `ctl_forward.rs`'s own module doc for why,
//! and this repo session's notes for the manual verification instead): the
//! `-R remote:local` UNIX-domain-socket forward itself, with a literal
//! (non-tilde-expanded) `/tmp/...` remote path, round-tripping bytes from
//! a remote-side connection to the local listener. Exercising that from a
//! remote shell command needs a UNIX-domain-socket-capable client tool
//! (`nc -U`, `socat`, or `python3`) that isn't a dependency of this crate's
//! test suite anywhere else, and whose availability/flag support varies
//! enough across environments (BSD vs. GNU `nc`, `nc -U` support) that a
//! committed automated test built on it would trade real coverage for
//! environment-dependent flakiness. It was instead verified manually,
//! directly against `/usr/sbin/sshd` + `ssh(1)` with the literal
//! `/tmp/isekai-ctl-...sock:$WORKDIR/local.sock` argument shape
//! `ctl_forward::prepare_ctl_forward`/`forward_option_args` produce, and
//! round-tripped bytes correctly with no tilde-expansion surprises.
//!
//! `isekai-ssh` has no `[lib]` target (bin-only crate, matching this
//! project's other `tests/*_e2e.rs` files), so this can't call
//! `ctl_forward::remote_command_arg` directly; it reconstructs the same
//! prefix shape ssh(1)-side instead, cross-checked against that function's
//! own unit test (`remote_command_exports_the_remote_path_and_execs_a_login_shell`)
//! for the exact string content. This intentionally does *not* go through
//! the `isekai-ssh` binary itself or the full isekai bootstrap/QUIC stack
//! (`ctl-socket` only activates for interactive invocations with no
//! trailing remote command, which has no clean automated-test termination
//! signal) — see `real_sshd_bootstrap_e2e.rs` for that fuller apparatus.
//!
//! Mock-sshd spawn helpers below are duplicated from
//! `real_sshd_bootstrap_e2e.rs` per this crate's established
//! self-containment convention for `tests/*_e2e.rs` files.

use std::process::Stdio as StdStdio;
use std::time::Duration;

fn sshd_available() -> bool {
    std::path::Path::new("/usr/sbin/sshd").exists()
}

fn ssh_binary_available() -> bool {
    std::process::Command::new("ssh")
        .arg("-V")
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status()
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
}

fn generate_keypair(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, String) {
    let key_path = dir.join(name);
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
        .arg(&key_path)
        .status()
        .expect("failed to run ssh-keygen (expected alongside ssh(1))");
    assert!(status.success(), "ssh-keygen exited non-zero");
    let pub_text = std::fs::read_to_string(dir.join(format!("{name}.pub")))
        .expect("failed to read generated .pub file");
    (key_path, pub_text)
}

fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn current_username() -> String {
    let out = std::process::Command::new("whoami").output().expect("failed to run whoami");
    assert!(out.status.success(), "whoami exited non-zero");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

struct RealSshd {
    child: std::process::Child,
    port: u16,
}

impl Drop for RealSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_real_sshd(workdir: &std::path::Path, authorized_keys_pub: &str) -> RealSshd {
    let (host_key_path, _host_pub) = generate_keypair(workdir, "host_key");
    let authorized_keys_path = workdir.join("authorized_keys");
    std::fs::write(&authorized_keys_path, authorized_keys_pub).unwrap();

    let port = free_tcp_port();
    let sshd_config_path = workdir.join("sshd_config");
    let sshd_config = format!(
        "Port {port}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {host_key}\n\
         AuthorizedKeysFile {authorized_keys}\n\
         PidFile {pidfile}\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         UsePAM no\n\
         StrictModes no\n\
         LogLevel VERBOSE\n",
        host_key = host_key_path.display(),
        authorized_keys = authorized_keys_path.display(),
        pidfile = workdir.join("sshd.pid").display(),
    );
    std::fs::write(&sshd_config_path, sshd_config).unwrap();

    let log_path = workdir.join("sshd.log");
    let log_file = std::fs::File::create(&log_path).unwrap();
    let log_file_err = log_file.try_clone().unwrap();
    let child = std::process::Command::new("/usr/sbin/sshd")
        .arg("-f")
        .arg(&sshd_config_path)
        .arg("-D")
        .arg("-e")
        .stdin(StdStdio::null())
        .stdout(log_file)
        .stderr(log_file_err)
        .spawn()
        .expect("failed to spawn /usr/sbin/sshd");
    let mut sshd = RealSshd { child, port };

    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return sshd;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = sshd.child.kill();
    let _ = sshd.child.wait();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    panic!("real sshd never started listening on 127.0.0.1:{port}; log:\n{log}");
}

/// `export ISEKAI_CTL_SOCK=<path>; <rest>` — the exact prefix shape
/// `ctl_forward::remote_command_arg` produces (see that function's own
/// unit test for the precise string) — actually delivers the env var to
/// `<rest>` when sshd runs it via `$SHELL -c "..."`.
#[test]
fn exported_env_var_prefix_reaches_the_wrapped_remote_command() {
    if !sshd_available() || !ssh_binary_available() {
        eprintln!("skipping: /usr/sbin/sshd or ssh(1) not available in this environment");
        return;
    }
    let workdir = tempfile::tempdir().unwrap();
    let (client_key_path, client_pub) = generate_keypair(workdir.path(), "client_key");
    let sshd = spawn_real_sshd(workdir.path(), &client_pub);

    let marker_path = workdir.path().join("env-marker.txt");
    let fake_remote_path = "/tmp/isekai-ctl-e2e-fake-path.sock";
    // Same shape as `ctl_forward::remote_command_arg`'s
    // `export ISEKAI_CTL_SOCK={:?}; exec "${SHELL:-/bin/sh}" -i -l`, with
    // the `exec` tail replaced by a deterministic, non-interactive
    // diagnostic (an interactive login shell has no clean termination
    // signal for an automated test).
    let remote_command = format!(
        "export ISEKAI_CTL_SOCK={fake_remote_path:?}; printf '%s' \"$ISEKAI_CTL_SOCK\" > {:?}",
        marker_path.display()
    );

    let output = std::process::Command::new("ssh")
        .args([
            "-F",
            "/dev/null",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "BatchMode=yes",
            "-i",
            client_key_path.to_str().unwrap(),
            "-p",
            &sshd.port.to_string(),
        ])
        .arg(format!("{}@127.0.0.1", current_username()))
        .arg(&remote_command)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .expect("failed to run ssh");

    assert!(
        output.status.success(),
        "ssh exited non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let observed =
        std::fs::read_to_string(&marker_path).expect("remote command never wrote the marker file");
    assert_eq!(observed, fake_remote_path);
}
