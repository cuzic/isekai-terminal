//! Real-`sshd` verification of the one piece of Epic P
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic P, "リモート発ビルドトリガー") that a
//! unit test can't exercise: `ctl_forward.rs::push_result_file`'s recursive
//! `isekai-ssh <host> -- mkdir -p ... && cat > ...` invocation.
//! `push_result_file` resolves the binary to spawn via
//! `std::env::current_exe()`, which correctly self-references the real
//! `isekai-ssh` binary in production, but resolves to whatever test binary
//! happens to be running under `cargo test` — so `ctl_forward.rs`'s own
//! `#[cfg(test)]` module can only unit-test the remote-command *string*
//! (`build_push_remote_command`), not a real spawn. This file supplies that
//! missing coverage by driving the actual compiled `isekai-ssh` binary
//! (`env!("CARGO_BIN_EXE_isekai-ssh")`) against a real local `sshd`, the same
//! way `push_result_file` itself would be driven in production.
//!
//! `--isekai-direct` (`wrapper.rs::run_openssh_direct`) skips the whole
//! isekai-pipe/QUIC bootstrap stack and just execs real `ssh(1)` with the
//! given args verbatim — exactly what a non-interactive result-push
//! invocation needs, and exactly what `ctl_socket_forward_e2e.rs`'s plain
//! `ssh` call already established is reliable to automate (unlike the
//! interactive ctl-socket `-R` forward round trip itself, deliberately left
//! as manual verification there — same reasoning applies here).
//!
//! Mock-sshd spawn helpers below are duplicated from
//! `real_sshd_bootstrap_e2e.rs`/`ctl_socket_forward_e2e.rs` per this crate's
//! established self-containment convention for `tests/*_e2e.rs` files.

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

/// Drives the real `isekai-ssh` binary (not raw `ssh`) with
/// `--isekai-direct` plus exactly the remote-command shape
/// `ctl_forward.rs::build_push_remote_command` produces (cross-checked
/// against that function's own unit tests for the precise string), feeding
/// it a local file's bytes over stdin the same way `push_result_file` does —
/// proving the actual artifact-push mechanism works against a real `sshd`,
/// not just that its remote-command string looks right.
#[test]
fn isekai_ssh_direct_pushes_a_build_result_via_cat_redirect() {
    if !sshd_available() || !ssh_binary_available() {
        eprintln!("skipping: /usr/sbin/sshd or ssh(1) not available in this environment");
        return;
    }
    let workdir = tempfile::tempdir().unwrap();
    let (client_key_path, client_pub) = generate_keypair(workdir.path(), "client_key");
    let sshd = spawn_real_sshd(workdir.path(), &client_pub);

    let dest_dir = workdir.path().join("dest");
    let artifact_contents = b"pretend-this-is-a-compiled-windows-exe";
    let remote_command = format!("mkdir -p {} && cat > {}/app.exe", dest_dir.display(), dest_dir.display());

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_isekai-ssh"))
        .args([
            "--isekai-direct",
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
        ])
        .arg(sshd.port.to_string())
        .arg(format!("{}@127.0.0.1", current_username()))
        .arg(&remote_command)
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    {
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(artifact_contents).unwrap();
        // dropped here (end of block) -> stdin closes -> remote `cat` sees EOF
    }
    let output = child.wait_with_output().expect("failed to wait for isekai-ssh");
    assert!(
        output.status.success(),
        "isekai-ssh --isekai-direct exited non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let pushed = std::fs::read(dest_dir.join("app.exe")).expect("pushed result file was never created");
    assert_eq!(pushed, artifact_contents);
}
