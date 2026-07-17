//! Real-`sshd` bootstrap harness (`ISEKAI_PIPE_DESIGN.md` §8 Epic C).
//!
//! Every other SSH-bootstrap `*_e2e.rs` file in this crate
//! (`init_e2e.rs`/`wrapper_auto_bootstrap_e2e.rs`) drives a real `ssh(1)`
//! client against an in-process mock server (`russh::server`), and — for
//! the deployed binary itself — either a real `isekai-pipe serve` process
//! whose handshake is relayed through a stand-in shell script (`init_e2e.rs`,
//! forced by the `--relay` launch path needing a real CA-verified relay it
//! can't fake locally), or a stand-in script alone
//! (`wrapper_auto_bootstrap_e2e.rs`). None of them upload and remotely
//! execute the actual compiled `isekai-pipe` binary through a genuine
//! OpenSSH server. This file closes that gap: it spawns a real
//! `/usr/sbin/sshd` subprocess (temp host key, temp `sshd_config`, real
//! pubkey authentication) and drives the wrapper's `LaunchSpec::Direct`
//! auto-bootstrap path (no relay involved, so no CA-verification wall)
//! through it, uploading and launching the real `isekai-pipe` binary this
//! workspace just built.
//!
//! `sshd` here runs unprivileged as this test process's own OS user (no
//! root available in CI) — an authenticated session therefore runs as that
//! same real user with that user's real `$HOME`, unlike the `russh` mock
//! (which sets `HOME` on the `sh -c` it spawns directly). To avoid ever
//! touching the real `$HOME`, every remote command this test triggers is
//! confined to system `/tmp` (`mktemp -d`, used internally by
//! `OpenSshBackend`) and an explicit `#@isekai remote-path` pointed at a
//! path inside this test's own tempdir — nothing here ever expands `~`.
//!
//! Scope: proves upload (base64-over-exec) + detached (`setsid`) remote
//! launch + handshake capture + `PersistentProfile` registration all work
//! against real OpenSSH server infrastructure, and that the uploaded binary
//! genuinely lands on disk byte-for-byte. It does *not* prove the deployed
//! process's QUIC endpoint stays reachable/serves real traffic after the
//! bootstrapping SSH session exits (`serve --target 127.0.0.1:22` on a real
//! machine would need real root/port-22 access to prove out that leg) —
//! that data-plane path is covered separately, against a directly-spawned
//! `serve` process, by `isekai-pipe/tests/connect_stun_fallback_e2e.rs`.
//! Packet-loss/latency resilience and network-namespace topology testing
//! live in `tests/netlab/` (repo root), not here.

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
/// `current_exe()` (matches `init_e2e.rs`'s helper of the same name —
/// duplicated per this crate's self-contained-test-file convention).
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
/// key and temp `sshd_config`, blocking until it accepts TCP connections
/// (sshd itself has no "ready" signal beyond actually listening).
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
    // Tracked by `RealSshd` from this point on, so its `Drop` reaps the
    // process on every exit path below — including the readiness-timeout
    // panic, which would otherwise leak a zombie `sshd` (clippy's
    // `zombie_processes` lint caught this in an earlier draft that only
    // wrapped the child on the success path).
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

/// Writes the throwaway config the *real `ssh(1)` subprocess* uses (`Host`
/// blocks for both the logical alias and the literal `127.0.0.1` — the
/// wrapper's auto-bootstrap step dials the *resolved* bootstrap-candidate
/// address directly, `wrapper.rs::bootstrap_and_register`, not the alias),
/// plus a PATH-shimmed `ssh` that always injects `-F <this config>` —
/// matching `wrapper_auto_bootstrap_e2e.rs::shim_ssh_with_bootstrap_config`'s
/// proven pattern (duplicated here rather than shared, per this crate's
/// self-contained-test-file convention).
///
/// This is deliberately a *different* file from `$HOME/.ssh/config`:
/// `isekai-ssh`'s own `#@isekai` directive parser (`wrapper.rs::config_roots`)
/// reads `$HOME/.ssh/config` directly (or whatever `-F` *isekai-ssh itself*
/// was given, which this test never passes), while the real `ssh(1)`
/// subprocess this shim wraps only ever sees this throwaway file via `-F` —
/// putting `#@isekai` lines here would have no effect (`Epic C`'s first draft
/// of this test made exactly that mistake).
fn shim_ssh_for_real_sshd(
    tmp: &std::path::Path,
    alias: &str,
    sshd_port: u16,
    username: &str,
    client_key_path: &std::path::Path,
) -> (PathBuf, std::ffi::OsString) {
    let config_path = tmp.join("ssh_config_bootstrap");
    let config = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {sshd_port}\n\
         \x20\x20\x20\x20User {username}\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n\
         \n\
         Host 127.0.0.1\n\
         \x20\x20\x20\x20Port {sshd_port}\n\
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
async fn wrapper_auto_bootstraps_the_real_binary_over_a_real_sshd() {
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
    let sshd = spawn_real_sshd(tmp.path(), &client_pub);
    let username = current_username();

    let remote_binary_path = tmp.path().join("remote-target").join("isekai-pipe");
    let (_bin_dir, path_env) = shim_ssh_for_real_sshd(tmp.path(), "real-sshd-host", sshd.port, &username, &client_key_path);

    let real_isekai_pipe_binary = std::fs::read(isekai_pipe_bin_path()).unwrap();
    let real_isekai_pipe_sha256 = hex_sha256(&real_isekai_pipe_binary);

    let client_home = tmp.path().join("client-home");
    let client_ssh_dir = client_home.join(".ssh");
    std::fs::create_dir_all(&client_ssh_dir).unwrap();
    // `isekai-ssh`'s own `#@isekai` directive parser reads this file
    // directly (see `shim_ssh_for_real_sshd`'s docs) — `remote-path` is an
    // absolute path (never `~`-relative) so uploading/launching never
    // depends on the real sshd session's real `$HOME`.
    std::fs::write(
        client_ssh_dir.join("config"),
        format!("Host real-sshd-host\n    #@isekai remote-path {}\n", remote_binary_path.display()),
    )
    .unwrap();

    let verbose_log_path = client_home.join("isekai-ssh-verbose-test.log");
    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(isekai_pipe_bin_path())
        .arg("real-sshd-host")
        .env("HOME", &client_home)
        // Verbose bootstrap-progress messages (including "Registered ...
        // in ...", which this test watches for below) now default to
        // `isekai-ssh`'s own log file (`log_file.rs::log_line_verbose!`)
        // rather than stderr — point it at a known path so the polling
        // loop below can check it directly instead of scanning stderr.
        .env("ISEKAI_PIPE_LOG_FILE", &verbose_log_path)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();

    // The wrapper proceeds to exec a real `ssh` for the actual login after
    // bootstrap succeeds; that attempt has nothing meaningful to do (the
    // deployed `isekai-pipe serve --target 127.0.0.1:22` can't reach a real
    // sshd on the privileged port 22 from this unprivileged test) and will
    // eventually fail/hang on its own — this test only cares that
    // bootstrap+registration completed first.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        // 90s (not this file's previous 20s): `OpenSshBackend::install_and_launch`
        // now does the upload+reuse-check+launch as a single combined ssh(1)
        // exec (`crate::reuse`'s module docs in `isekai-bootstrap`), so no
        // stderr line at all is emitted between "deploying..." and
        // "Registered" while that whole exec is in flight — unlike the two
        // separate ssh(1) round trips this used to be split across. Timed
        // directly against this exact fixture (real sshd, this workspace's
        // own ~180MB debug `isekai-pipe` binary): a bare `install_and_start`
        // call reliably completes within ~25s, but this sandbox's own
        // variable build/agent load (`Concurrent agents on main` — several
        // other agents routinely run `cargo` here too) pushed the old 20s
        // budget into consistent, reproducible failures even though nothing
        // was actually wrong. 90s matches this project's own established
        // convention for opt-in real e2e tests under load (generous
        // polling/timeouts rather than tightening around a happy-path
        // measurement).
        match tokio::time::timeout(Duration::from_secs(90), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                let verbose_log_has_registered =
                    std::fs::read_to_string(&verbose_log_path).map(|s| s.contains("Registered")).unwrap_or(false);
                if line.contains("Registered") || verbose_log_has_registered {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report profile registration");

    // The uploaded binary must exist byte-for-byte, at the exact path this
    // test told the wrapper to use — verifiable directly since "remote" and
    // "local" are the same machine here, unlike every other bootstrap e2e
    // test in this crate (which never upload a real binary at all).
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
    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir, "real-sshd-host:22")
        .unwrap()
        .expect("expected a profile for real-sshd-host:22");
    let legacy_relay = profile.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert!(
        legacy_relay.helper_addr.starts_with("127.0.0.1:"),
        "direct-by-bootstrap-host candidate should be on 127.0.0.1, got {}",
        legacy_relay.helper_addr
    );
    assert_eq!(profile.server_identity.cert_sha256_hex.len(), 64, "cert_sha256 should be a 32-byte hex digest");
    assert!(!legacy_relay.session_secret_b64.is_empty(), "session_secret should be non-empty");
}
