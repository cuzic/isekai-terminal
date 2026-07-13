//! Real 2-hop `--via` bootstrap E2E (`ISEKAI_PIPE_DESIGN.md` §8 Epic K's
//! accept criterion: "Epic Cのharnessを使った最低2-hop構成のE2Eを受け入れ条件にする").
//!
//! Extends `real_sshd_bootstrap_e2e.rs`'s single-`sshd` harness pattern
//! (real `/usr/sbin/sshd`, temp host key, real pubkey auth, real uploaded
//! `isekai-pipe` binary) to *two* real `sshd` instances on `127.0.0.1` at
//! different ports — a bastion and the final target — connected through
//! `ssh(1)`'s own `-J bastion,target`-shape jump chain
//! (`OpenSshBackend::install_and_start`'s `via: &[JumpSpec]`,
//! `isekai-bootstrap-plan::BootstrapPlan::validate_jump_chain`). Per this
//! crate's self-contained-test-file convention (each `*_e2e.rs` duplicates
//! its own mock/real-server helpers rather than sharing a `tests/common/`),
//! this file duplicates rather than imports `real_sshd_bootstrap_e2e.rs`'s
//! helpers.
//!
//! The `via` hop is never given as a raw CLI/directive value here — it's
//! sourced from the *real* OpenSSH `ProxyJump` keyword (a `Host` block in
//! the shim `-F` config), resolved through `ssh -G` exactly like a real
//! user's `~/.ssh/config` would be
//! (`wrapper.rs::defaults_bootstrap_candidate_from_ssh_g`), rather than an
//! explicit `#@isekai bootstrap-candidate ... via=...` directive — proving
//! the ordinary `ProxyJump`-in-`~/.ssh/config` path this feature exists for,
//! not just the directive escape hatch.

use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;

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

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh").arg("-c").arg("command -v ssh").output().expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// Locates a sibling workspace package's binary by walking up from
/// `current_exe()` (duplicated from `real_sshd_bootstrap_e2e.rs` per this
/// crate's self-contained-test-file convention).
fn sibling_bin_path(package: &str, bin_name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    path.push(bin_name);

    if !path.exists() {
        eprintln!("{bin_name} binary not found at {path:?}; building it now");
        let mut cmd = std::process::Command::new(env!("CARGO"));
        cmd.args(["build", "-p", package]);
        if is_release {
            cmd.arg("--release");
        }
        let status = cmd.status().unwrap_or_else(|_| panic!("failed to invoke `cargo build -p {package}`"));
        assert!(status.success(), "`cargo build -p {package}` failed");
        assert!(path.exists(), "{bin_name} binary still missing at {path:?} after building it");
    }
    path
}

fn isekai_pipe_bin_path() -> PathBuf {
    sibling_bin_path("isekai-pipe", "isekai-pipe")
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn generate_keypair(dir: &std::path::Path, name: &str) -> (PathBuf, String) {
    let key_path = dir.join(name);
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
        .arg(&key_path)
        .status()
        .expect("failed to run ssh-keygen (expected alongside ssh(1))");
    assert!(status.success(), "ssh-keygen exited non-zero");
    let pub_text = std::fs::read_to_string(dir.join(format!("{name}.pub"))).expect("failed to read generated .pub file");
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

/// Spawns a real `/usr/sbin/sshd -D -e` against a freshly generated host
/// key and temp `sshd_config`, blocking until it accepts TCP connections.
/// `label` disambiguates the per-instance files (host key, authorized_keys,
/// pidfile, log) when multiple instances share one `workdir` — this file's
/// whole point is running two of these at once.
fn spawn_real_sshd(workdir: &std::path::Path, label: &str, authorized_keys_pub: &str) -> RealSshd {
    let (host_key_path, _host_pub) = generate_keypair(workdir, &format!("host_key_{label}"));
    let authorized_keys_path = workdir.join(format!("authorized_keys_{label}"));
    std::fs::write(&authorized_keys_path, authorized_keys_pub).unwrap();

    let port = free_tcp_port();
    let sshd_config_path = workdir.join(format!("sshd_config_{label}"));
    let sshd_config = format!(
        "Port {port}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {host_key}\n\
         AuthorizedKeysFile {authorized_keys}\n\
         PidFile {pidfile}\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         AllowTcpForwarding yes\n\
         UsePAM no\n\
         StrictModes no\n\
         LogLevel VERBOSE\n",
        host_key = host_key_path.display(),
        authorized_keys = authorized_keys_path.display(),
        pidfile = workdir.join(format!("sshd_{label}.pid")).display(),
    );
    std::fs::write(&sshd_config_path, sshd_config).unwrap();

    let log_path = workdir.join(format!("sshd_{label}.log"));
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
    // Tracked by `RealSshd` from this point on so `Drop` reaps the process
    // on every exit path, including the readiness-timeout panic below.
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
    panic!("real sshd ({label}) never started listening on 127.0.0.1:{port}; log:\n{log}");
}

/// Writes the throwaway `-F` config the *real `ssh(1)` subprocess* uses, and
/// a PATH-shimmed `ssh` that always injects it — matching
/// `real_sshd_bootstrap_e2e.rs::shim_ssh_for_real_sshd`'s proven pattern.
/// Unlike the single-hop version, this wires `ProxyJump` on the `<alias>`
/// block so `ssh -G <alias>` (which `wrapper.rs::defaults_bootstrap_candidate_from_ssh_g`
/// reads) reports it.
///
/// The final destination the deploy step actually dials is the *alias*
/// itself, not `127.0.0.1` — `wrapper.rs::bootstrap_and_register` (see its
/// "prefer the original alias over the `ssh -G`-resolved host" comment)
/// deliberately passes `candidate.alias` as the `ssh(1)` destination so a
/// real user's own `Host <alias>` block (`IdentityFile`/`ProxyCommand`/etc.)
/// still applies. Only the `-J` jump-hop argument (built from the parsed
/// `ssh -G` `proxyjump` output) is the literal `127.0.0.1`. So the
/// `IdentityFile`/`StrictHostKeyChecking no`/`UserKnownHostsFile /dev/null`
/// bypass has to apply to *both* patterns — `Host {alias} 127.0.0.1` (one
/// `Host` line, two patterns) rather than a `Host 127.0.0.1`-only block,
/// otherwise the final-hop connection (destination = the alias) falls
/// through to this process's real `~/.ssh/known_hosts` and `BatchMode=yes`
/// aborts it with "Host key verification failed." (found via an actual
/// repro: this previously read `Host 127.0.0.1` only, which does not match
/// the literal string `{alias}` used as the destination argument).
fn shim_ssh_for_two_hop_real_sshd(
    tmp: &std::path::Path,
    alias: &str,
    bastion_port: u16,
    target_port: u16,
    username: &str,
    client_key_path: &std::path::Path,
) -> (PathBuf, std::ffi::OsString) {
    let config_path = tmp.join("ssh_config_bootstrap");
    let config = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {target_port}\n\
         \x20\x20\x20\x20User {username}\n\
         \x20\x20\x20\x20ProxyJump {username}@127.0.0.1:{bastion_port}\n\
         \n\
         Host {alias} 127.0.0.1\n\
         \x20\x20\x20\x20User {username}\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n",
        key = client_key_path.display(),
    );
    std::fs::write(&config_path, config).unwrap();

    let bin_dir = tmp.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim_path = bin_dir.join("ssh");
    let shim = format!(
        "#!/bin/sh\nexec {real_ssh} -F {config} \"$@\"\n",
        real_ssh = real_ssh_path().display(),
        config = config_path.display(),
    );
    std::fs::write(&shim_path, shim).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path_env = {
        let mut paths = vec![bin_dir.clone()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        std::env::join_paths(paths).unwrap()
    };
    (bin_dir, path_env)
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstraps_through_a_real_two_hop_via_chain() {
    if !sshd_available() {
        eprintln!("skipping: /usr/sbin/sshd not available in this environment");
        return;
    }
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (client_key_path, client_pub) = generate_keypair(tmp.path(), "client_key");
    // Same key/user authenticates to both hops — this test's concern is the
    // jump-chain plumbing, not per-hop credential differentiation.
    let bastion = spawn_real_sshd(tmp.path(), "bastion", &client_pub);
    let target = spawn_real_sshd(tmp.path(), "target", &client_pub);
    let username = current_username();

    let remote_binary_path = tmp.path().join("remote-target").join("isekai-pipe");
    let (_bin_dir, path_env) =
        shim_ssh_for_two_hop_real_sshd(tmp.path(), "real-sshd-multihop-host", bastion.port, target.port, &username, &client_key_path);

    let real_isekai_pipe_binary = std::fs::read(isekai_pipe_bin_path()).unwrap();
    let real_isekai_pipe_sha256 = hex_sha256(&real_isekai_pipe_binary);

    let client_home = tmp.path().join("client-home");
    let client_ssh_dir = client_home.join(".ssh");
    std::fs::create_dir_all(&client_ssh_dir).unwrap();
    // The wrapper's own `#@isekai` directive parser reads this file
    // directly (separate from the shim `-F` config above, which only the
    // underlying real `ssh(1)` subprocess sees) — `remote-path` is absolute
    // so uploading/launching never depends on the real sshd session's real
    // `$HOME`. No `#@isekai bootstrap-candidate ... via=...` directive here
    // on purpose: the jump hop comes from the plain `ProxyJump` OpenSSH
    // keyword above, resolved through `ssh -G` like a real user's config.
    std::fs::write(
        client_ssh_dir.join("config"),
        format!("Host real-sshd-multihop-host\n    #@isekai remote-path {}\n", remote_binary_path.display()),
    )
    .unwrap();

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(isekai_pipe_bin_path())
        .arg("real-sshd-multihop-host")
        .env("HOME", &client_home)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();

    // As in `real_sshd_bootstrap_e2e.rs`: the wrapper proceeds to exec a
    // real `ssh` for the actual login after bootstrap succeeds, which has
    // nothing meaningful to reach and eventually fails/hangs on its own —
    // this test only cares that bootstrap+registration completed first,
    // *through* the jump chain.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    let mut stderr_log = String::new();
    for _ in 0..200 {
        let mut line = String::new();
        // 90s — see `real_sshd_bootstrap_e2e.rs`'s identical comment:
        // `OpenSshBackend::install_and_launch` now does upload+reuse-check+
        // launch as a single combined ssh(1) exec, so no stderr line at all
        // appears between "deploying..." and "Registered" while that whole
        // exec (here, through a 2-hop `-J` chain, so even more latency than
        // the single-hop fixture) is in flight.
        match tokio::time::timeout(Duration::from_secs(90), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                stderr_log.push_str(&line);
                if line.contains("Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report profile registration; stderr so far:\n{stderr_log}");

    // The uploaded binary must exist byte-for-byte on the *target* host, at
    // the exact path this test told the wrapper to use, having travelled
    // through the bastion hop rather than being deployed to it.
    let uploaded = std::fs::read(&remote_binary_path)
        .unwrap_or_else(|e| panic!("expected the real isekai-pipe binary to be uploaded at {remote_binary_path:?}: {e}"));
    assert_eq!(hex_sha256(&uploaded), real_isekai_pipe_sha256, "uploaded binary must match the local isekai-pipe binary byte-for-byte");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&remote_binary_path).unwrap().permissions().mode();
        assert!(mode & 0o100 != 0, "uploaded binary must be executable, got mode {mode:o}");
    }

    let profiles_dir = client_home.join(".local").join("state").join("isekai").join("profiles");
    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir, "real-sshd-multihop-host:22")
        .unwrap()
        .expect("expected a profile for real-sshd-multihop-host:22");
    let legacy_relay = profile.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert!(
        legacy_relay.helper_addr.starts_with("127.0.0.1:"),
        "direct-by-bootstrap-host candidate should be on 127.0.0.1 (the target, dialed directly — bootstrap-time deploy never re-dials through the jump chain for the direct route), got {}",
        legacy_relay.helper_addr
    );
    assert_eq!(profile.server_identity.cert_sha256_hex.len(), 64, "cert_sha256 should be a 32-byte hex digest");
    assert!(!legacy_relay.session_secret_b64.is_empty(), "session_secret should be non-empty");
    // `ssh -G`'s `proxyjump` output bracket-wraps a literal IPv4 host when a
    // port is present (`user@[127.0.0.1]:port`, standard OpenSSH
    // formatting) — `isekai_trust::split_user_host_port` (used to parse
    // this into a `JumpSpec`) accepts that shape, and it's preserved
    // verbatim through `last_via` since that field is purely informational
    // display text, not re-parsed.
    assert_eq!(
        profile.last_via.as_deref(),
        Some(format!("{username}@[127.0.0.1]:{}", bastion.port).as_str()),
        "last_via should record the resolved jump hop"
    );

    // Sanity check this test actually exercised two *distinct* sshd
    // instances rather than accidentally routing everything to one.
    assert_ne!(bastion.port, target.port);
}
