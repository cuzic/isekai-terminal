//! End-to-end test for the wrapper's auto-bootstrap path
//! (`wrapper.rs::bootstrap_and_register`, `archive/ISEKAI_PIPE_MIGRATION.md` P4's
//! last item): a never-before-seen destination triggers a
//! `direct-by-bootstrap-host` deploy over a real `ssh(1)` subprocess,
//! prompts for confirmation on stderr, and — on `y` — registers the trust
//! store entry the wrapper (and subsequent plain `isekai-ssh <destination>`
//! invocations) will read from then on.
//!
//! Mirrors `init_e2e.rs`'s mock-sshd/stand-in-helper-script plumbing (this
//! crate's convention: one self-contained e2e file per scenario, see
//! `stdout_cleanliness.rs`'s module docs for why that's duplicated rather
//! than shared).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::decode_handshake_json;
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::Command as TokioCommand;

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

/// Renders `path` with forward slashes, safe to embed inside a `#!/bin/sh`
/// script body that will be interpreted by `sh -c`/`sh <script>` (the
/// stand-in "remote helper" scripts below, run inside the mock sshd's
/// `exec_request` handler). Needed only on Windows: `Path::display()`
/// there renders `\`-separated components, and an unquoted `\` inside a
/// POSIX shell script is an escape character — embedding a raw Windows
/// path like `echo "$@" > {path}` silently mangles it (confirmed via a
/// real `test-windows` CI failure: the script ran, but wrote its argv log
/// to a corrupted path, not the one the test later reads from). Windows'
/// own filesystem APIs accept forward slashes exactly as well as
/// backslashes, so this is a lossless substitution, not a real path
/// translation. A no-op on Unix, where paths are already `/`-separated.
fn posix_safe_path(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
}

#[derive(Clone)]
struct FakeShellServer {
    home: PathBuf,
    accepted_client_key: PublicKey,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        FakeShellHandler {
            home: self.home.clone(),
            accepted_client_key: self.accepted_client_key.clone(),
            stdin_senders: std::collections::HashMap::new(),
        }
    }
}

struct FakeShellHandler {
    home: PathBuf,
    accepted_client_key: PublicKey,
    stdin_senders: std::collections::HashMap<ChannelId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

#[async_trait::async_trait]
impl server::Handler for FakeShellHandler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth, Self::Error> {
        if public_key.key_data() == self.accepted_client_key.key_data() {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject { proceed_with_methods: None })
        }
    }

    async fn channel_open_session(&mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        let home = self.home.clone();

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
            .stdin(StdStdio::piped())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .spawn()
            .expect("mock sshd failed to spawn sh -c for exec_request");

        let mut child_stdin = child.stdin.take().expect("stdin piped");
        let mut child_stdout = child.stdout.take().expect("stdout piped");
        let mut child_stderr = child.stderr.take().expect("stderr piped");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        self.stdin_senders.insert(channel, tx);

        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if child_stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = child_stdin.shutdown().await;
        });

        tokio::spawn(async move {
            let (stdout_res, stderr_res, wait_res) =
                tokio::join!(read_all(&mut child_stdout), read_all(&mut child_stderr), child.wait());
            if let Ok(out) = stdout_res {
                if !out.is_empty() {
                    let _ = handle.data(channel, CryptoVec::from(out)).await;
                }
            }
            if let Ok(err) = stderr_res {
                if !err.is_empty() {
                    let _ = handle.extended_data(channel, 1, CryptoVec::from(err)).await;
                }
            }
            let code = wait_res.ok().and_then(|s| s.code()).unwrap_or(1) as u32;
            let _ = handle.exit_status_request(channel, code).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });

        session.channel_success(channel)?;
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut ServerSession) -> Result<(), Self::Error> {
        if let Some(tx) = self.stdin_senders.get(&channel) {
            let _ = tx.send(data.to_vec());
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut ServerSession) -> Result<(), Self::Error> {
        self.stdin_senders.remove(&channel);
        Ok(())
    }
}

async fn spawn_fake_ssh_server(home: PathBuf, accepted_client_key: PublicKey) -> SocketAddr {
    let keypair = Ed25519Keypair::from_seed(&[11u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let config = std::sync::Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { home, accepted_client_key };
    tokio::spawn(async move {
        use server::Server as _;
        let _ = sh.run_on_socket(config, &listener).await;
    });
    addr
}

fn generate_client_keypair(dir: &std::path::Path) -> (PathBuf, PublicKey) {
    let key_path = dir.join("client_id_ed25519");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
        .arg(&key_path)
        .status()
        .expect("failed to run ssh-keygen");
    assert!(status.success(), "ssh-keygen exited non-zero");

    let pub_path = dir.join("client_id_ed25519.pub");
    let pub_text = std::fs::read_to_string(&pub_path).expect("failed to read generated .pub file");
    let public_key = PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key");
    (key_path, public_key)
}

#[cfg(unix)]
fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v ssh")
        .output()
        .expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// Windows counterpart of the `sh -c "command -v ssh"` above. Deliberately
/// *not* the same implementation: under Git Bash/MSYS2, `command -v ssh`
/// resolves to a POSIX-style path (e.g. `/usr/bin/ssh`), which
/// `ssh_test_shim` (a real Win32 `Command`, not a shell) can't invoke
/// directly. `where.exe` (a built-in Windows command, distinct from Git
/// Bash's `which`) resolves via `%PATH%`/`%PATHEXT%` and returns a native
/// `C:\...\ssh.exe`-shaped path instead.
#[cfg(windows)]
fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("where").arg("ssh.exe").output().expect("failed to run `where ssh.exe`");
    assert!(out.status.success(), "ssh.exe not found on PATH");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let first = stdout.lines().next().expect("`where ssh.exe` produced no output");
    PathBuf::from(first.trim())
}

/// Everything needed to point `isekai-ssh` (and everything it internally
/// spawns — `wrapper.rs::run_ssh_once`/`resolve_openssh_effective_config`
/// *and* `isekai-bootstrap::OpenSshBackend`'s own deploy dial) at a stand-in
/// `ssh(1)` that always injects `-F <config_path>` ahead of whatever real
/// `ssh(1)` arguments it's given, without touching the test runner's actual
/// `~/.ssh/config`.
struct SshShim {
    /// Pass as `--isekai-ssh-path` when spawning `isekai-ssh`.
    path: PathBuf,
    /// Extra env vars to set on the spawned `isekai-ssh` process (empty on
    /// Unix; see `ssh_test_shim`'s module docs for why Windows needs them).
    extra_env: Vec<(&'static str, PathBuf)>,
}

/// Unix: a `#!/bin/sh` script written into `bin_dir` (needs `+x`, since
/// `Command::new(&plan.openssh_path)` execs it directly, and the kernel's
/// own `exec()` understands the shebang regardless of the file's
/// extension). `--isekai-ssh-path` isn't strictly required here (PATH
/// alone would find it), but is passed anyway so callers don't need a
/// second, platform-specific code path — see the Windows variant below for
/// why *it* genuinely can't rely on PATH-shadowing alone.
#[cfg(unix)]
fn write_ssh_shim(bin_dir: &std::path::Path, real_ssh: &std::path::Path, config_path: &std::path::Path) -> SshShim {
    let shim_path = bin_dir.join("ssh");
    let shim =
        format!("#!/bin/sh\nexec {real_ssh} -F {config} \"$@\"\n", real_ssh = real_ssh.display(), config = config_path.display());
    std::fs::write(&shim_path, shim).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    SshShim { path: shim_path, extra_env: Vec::new() }
}

/// Windows: no file is written at all — the shim is `ssh_test_shim`, a real
/// compiled `.exe` (`src/bin/ssh_test_shim.rs`, already built by `cargo
/// test` itself, found via `CARGO_BIN_EXE_ssh_test_shim`). Two earlier
/// approaches were tried and abandoned (real `test-windows` CI failures at
/// each step, see git history and `ssh_test_shim`'s own module docs for the
/// full story):
/// 1. A bare POSIX shebang script named `ssh`: `Command::new`/`CreateProcessW`
///    doesn't interpret shebangs, and doesn't even find an extension-less
///    file from a bare name.
/// 2. A `ssh.cmd` batch-file pass-through, invoked via `--isekai-ssh-path`
///    (bare `Command::new("ssh")` only implicitly resolves `.exe`, not
///    `.cmd`, so a `--isekai-ssh-path` pointing directly at the shim was
///    also needed then, unlike Unix's PATH-shadowing): this got invoked
///    correctly, but `std::process::Command`'s Windows batch-file
///    argument-safety validation (CVE-2024-24576/"BatBadBut") rejects any
///    argument containing `\r`/`\n` outright — and the real bootstrap
///    deploy step's remote command is exactly such a multi-line string.
///
/// A genuine `.exe` sidesteps both problems: it's never treated as a batch
/// file (no argument-safety special-casing applies), and ordinary Win32
/// argv passing handles embedded newlines within a single argument fine.
#[cfg(windows)]
fn write_ssh_shim(_bin_dir: &std::path::Path, real_ssh: &std::path::Path, config_path: &std::path::Path) -> SshShim {
    let shim_path = PathBuf::from(env!("CARGO_BIN_EXE_ssh_test_shim"));
    expose_msys_dll_next_to(&shim_path, real_ssh);
    SshShim {
        path: shim_path,
        extra_env: vec![
            ("ISEKAI_SSH_TEST_SHIM_REAL_SSH", real_ssh.to_path_buf()),
            ("ISEKAI_SSH_TEST_SHIM_CONFIG", config_path.to_path_buf()),
        ],
    }
}

/// `wrapper.rs::proxy_command` decides whether the *real* connect step's
/// `ProxyCommand` needs POSIX single-quoting (`wrapper.rs::is_posix_shell_ssh`)
/// by checking for `msys-2.0.dll`/`cygwin1.dll` next to the *resolved*
/// `--isekai-ssh-path` binary. That binary is `ssh_test_shim.exe` here, not
/// the real MSYS2-hosted `ssh.exe` it execs internally — so without this,
/// the check incorrectly concludes "not POSIX-shell", skips the quoting
/// that assumption requires, and the connect step's embedded Windows path
/// gets its backslashes silently eaten when the real (POSIX-shell) ssh
/// actually execs the `ProxyCommand` via its own `sh -c` (confirmed via a
/// real `test-windows` CI failure on `wrapper_stale_trust_auto_recovery_e2e.rs`:
/// `sh -c` reported `exec: <path with every `\` stripped>: not found`, the
/// same class of bug `posix_safe_path` fixes for a different embedding
/// site). Copying the same companion DLL next to the shim makes that
/// detection see the same thing it would for the real `ssh.exe`. A no-op
/// (and harmless to call repeatedly/concurrently across tests sharing this
/// crate's `target/`) if neither DLL exists next to `real_ssh` at all.
#[cfg(windows)]
fn expose_msys_dll_next_to(shim_path: &std::path::Path, real_ssh: &std::path::Path) {
    let Some(real_ssh_dir) = real_ssh.parent() else { return };
    let Some(shim_dir) = shim_path.parent() else { return };
    for dll in ["msys-2.0.dll", "cygwin1.dll"] {
        let src = real_ssh_dir.join(dll);
        if src.is_file() {
            let _ = std::fs::copy(&src, shim_dir.join(dll));
        }
    }
}

/// `--isekai-ssh-path <shim.path>` plus `shim.extra_env` — see `SshShim`'s
/// docs. `isekai-bootstrap::OpenSshBackend`'s deploy dial only honors
/// `--isekai-ssh-path` because `wrapper.rs::bootstrap_and_register` now
/// threads `plan.openssh_path` into `OpenSshBackend::with_ssh_program`
/// (previously it silently used its own bare-`"ssh"` default regardless of
/// this flag — a real, if previously harmless-on-Unix, inconsistency this
/// Windows work surfaced; see that function's own doc comment).
fn ssh_shim_args_and_env(shim: &SshShim) -> (Vec<std::ffi::OsString>, Vec<(&'static str, PathBuf)>) {
    (vec!["--isekai-ssh-path".into(), shim.path.clone().into()], shim.extra_env.clone())
}

/// Same technique as `init_e2e.rs::shim_ssh_with_bootstrap_config`: a tiny
/// `ssh` shim ahead of the real one on `$PATH` that always injects `-F
/// <throwaway config>`, standing in for a real `~/.ssh/config` entry without
/// touching the test runner's actual home directory. Also used for the
/// wrapper's own internal `ssh -G` call (`wrapper.rs::resolve_openssh_effective_config`),
/// so both that call and the real `ssh` exec at the end of `run()` see the
/// same resolved config.
///
/// **Also writes the identical `Host` blocks to `home/.ssh/config`**
/// (creating the directory if needed), not just the shim-only throwaway
/// file: the Windows-native path (`isekai-ssh/src/native/connect.rs::prepare`
/// / `wrapper::resolve_for_native`) does its *own* full `openssh-config`
/// resolution directly against `$HOME/.ssh/config` — it never shells out to
/// `ssh(1)`, so it never sees the shim-only config the `-F` flag points at.
/// Before this, only the shim config carried `HostName`/`Port`/`IdentityFile`,
/// so `RusshBackend::resolve_hop` (which also calls `openssh_config::
/// resolve_default`, exactly mirroring what real `ssh(1)` does) found no
/// `HostName` for these fake test aliases and fell back to literally
/// DNS-resolving the alias string itself — confirmed via a real `test-windows`
/// CI failure (`No such host is known`) once the native path's own inline
/// auto-bootstrap was wired up. `extra_directive_lines` lets a caller append
/// `#@isekai ...` directive lines under the same `Host {alias}` block instead
/// of writing a second, HostName-less block separately (which is exactly the
/// gap that caused the failure).
fn shim_ssh_with_bootstrap_config(
    tmp: &std::path::Path,
    home: &std::path::Path,
    alias: &str,
    mock_sshd_addr: SocketAddr,
    key_path: &std::path::Path,
    extra_directive_lines: &str,
) -> (PathBuf, std::ffi::OsString, SshShim) {
    let host_block = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n\
         {extra_directive_lines}\n\
         # The wrapper's auto-bootstrap step dials the *resolved*
         # bootstrap-candidate address directly (`wrapper.rs::bootstrap_and_register`),
         # not the `{alias}` alias above, so it needs its own matching block
         # with the same test identity/trust settings.
         Host 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n",
        port = mock_sshd_addr.port(),
        key = key_path.display(),
    );

    let config_path = tmp.join("ssh_config_bootstrap");
    std::fs::write(&config_path, &host_block).unwrap();

    let home_ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&home_ssh_dir).unwrap();
    std::fs::write(home_ssh_dir.join("config"), &host_block).unwrap();

    let bin_dir = tmp.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim = write_ssh_shim(&bin_dir, &real_ssh_path(), &config_path);

    let path_env = {
        let mut paths = vec![bin_dir.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        std::env::join_paths(paths).unwrap()
    };
    (bin_dir, path_env, shim)
}

/// Regression test fixture for the alias-vs-resolved-`HostName` bug
/// (`wrapper.rs::BootstrapCandidate::alias`'s docs): unlike
/// `shim_ssh_with_bootstrap_config`, this deliberately has **no** second
/// `Host 127.0.0.1` block. Before the fix, `bootstrap_and_register` dialed
/// the `ssh -G`-resolved `HostName` (`127.0.0.1`) directly, which would
/// never match `Host {alias}` here — so `IdentityFile`/`User`/
/// `StrictHostKeyChecking` would all be silently dropped for that deploy
/// `ssh(1)` call, exactly the failure mode the real user hit with a custom
/// `IdentityFile` under their `Host <alias>` block. If `bootstrap_and_register`
/// regresses to dialing the resolved address again, this config gives it
/// nothing to authenticate with and the test times out/fails instead of
/// registering.
/// Also writes the identical `Host` block to `home/.ssh/config` — see
/// `shim_ssh_with_bootstrap_config`'s doc comment for why the native path
/// needs this in addition to the shim-only config.
fn shim_ssh_with_alias_only_bootstrap_config(
    tmp: &std::path::Path,
    home: &std::path::Path,
    alias: &str,
    mock_sshd_addr: SocketAddr,
    key_path: &std::path::Path,
) -> (PathBuf, std::ffi::OsString, SshShim) {
    let config = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n",
        port = mock_sshd_addr.port(),
        key = key_path.display(),
    );
    let config_path = tmp.join("ssh_config_bootstrap_alias_only");
    std::fs::write(&config_path, &config).unwrap();

    let home_ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&home_ssh_dir).unwrap();
    std::fs::write(home_ssh_dir.join("config"), &config).unwrap();

    let bin_dir = tmp.join("bin-alias-only");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim = write_ssh_shim(&bin_dir, &real_ssh_path(), &config_path);

    let path_env = {
        let mut paths = vec![bin_dir.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        std::env::join_paths(paths).unwrap()
    };
    (bin_dir, path_env, shim)
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

/// Mirrors `isekai_pipe_core::profile::sanitize_filename_component`'s `:` ->
/// `%3A` escaping (private to that crate) — every key this file uses is a
/// plain `host:port` string, so replicating just that one substitution is
/// enough to predict the on-disk filename `write_persistent_profile`
/// actually produces.
fn profile_path_under(home: &std::path::Path, key: &str) -> PathBuf {
    profiles_dir_under(home).join(format!("{}.json", key.replace(':', "%3A")))
}

/// Verbose bootstrap-progress messages (including the "Registered ... in
/// ..." confirmation these tests watch for) now go to `isekai-ssh`'s
/// default log file (`log_file.rs::log_line_verbose!`) rather than stderr
/// — these tests point that log file at a known path under the test's own
/// `home` (`.env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))`)
/// and poll it here instead of scanning stderr for the line.
fn verbose_log_path_under(home: &std::path::Path) -> PathBuf {
    home.join("isekai-ssh-verbose-test.log")
}

fn verbose_log_contains(path: &std::path::Path, needle: &str) -> bool {
    std::fs::read_to_string(path).map(|contents| contents.contains(needle)).unwrap_or(false)
}

const VALID_HANDSHAKE_JSON: &str = r#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]}"#;

/// `#20a-4`: every real `OpenSshBackend` launch now sends a
/// `BootstrapRequestV2` over stdin, so a compliant `isekai-pipe serve`
/// always echoes back a `BootstrapReportV2` envelope (never a bare
/// `HandshakeJson`) on stdout — the stand-in helper scripts below must match
/// that shape. `session_id`/`bootstrap_attempt_id` here are arbitrary valid
/// hex; these tests don't correlate them against the request the fake
/// script actually received.
const VALID_BOOTSTRAP_REPORT_JSON: &str = r#"{"v":2,"session_id":"00000000000000000000000000000000","bootstrap_attempt_id":"11111111111111111111111111111111","handshake":{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]}}"#;

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstraps_an_untrusted_destination_on_confirmation() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env, shim) =
        shim_ssh_with_bootstrap_config(tmp.path(), &home, "auto-bootstrap-host", mock_sshd_addr, &key_path, "");

    // Stand-in for the isekai-helper binary: ignores its args, just emits
    // one line of valid handshake JSON — same technique as
    // `init_e2e.rs`/`isekai-bootstrap/tests/openssh_e2e.rs`.
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let profile_path = profile_path_under(&home, "auto-bootstrap-host:22");
    assert!(!profile_path.exists(), "profile must not exist before this test runs");

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("auto-bootstrap-host")
        .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // On the native/Windows path, `RusshBackend`'s own SSH host-key TOFU
    // prompt ("Are you sure you want to continue connecting (yes/no)?")
    // fires *before* the app-level "Trust this isekai-helper...? [y/N]"
    // prompt this "y\n" answers — unlike the Unix/`ssh(1)` path, which
    // never asks that first question at all (`StrictHostKeyChecking no` in
    // the shim config suppresses it). See `RusshBackend`'s module docs
    // (`isekai-bootstrap/src/russh_backend.rs`) for why there's no
    // `StrictHostKeyChecking`-equivalent knob to suppress it there too.
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\ny\n" } else { b"y\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    // The wrapper proceeds to exec a real `ssh` with `ProxyCommand isekai-pipe
    // connect ...` after bootstrap succeeds; that connect attempt has nothing
    // real to talk to (the stand-in helper script already exited after
    // printing its one line) and will eventually fail/hang on its own, which
    // is fine — this test only cares that bootstrap+registration completed
    // first, so it reads stderr until the registration line shows up rather
    // than waiting for the whole process to exit.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") || verbose_log_contains(&verbose_log_path_under(&home), "Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");
    assert!(profile_path.exists(), "expected profile to be written at {profile_path:?}");

    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), "auto-bootstrap-host:22")
        .unwrap()
        .expect("expected a profile for auto-bootstrap-host:22");
    let legacy_relay = profile.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert_eq!(legacy_relay.helper_addr, "127.0.0.1:45231");
    let handshake = decode_handshake_json(VALID_HANDSHAKE_JSON.as_bytes()).unwrap();
    assert_eq!(profile.server_identity.cert_sha256_hex, handshake.cert_sha256());
    assert_eq!(profile.update_policy, isekai_trust::UpdatePolicy::ExactDigestOnly);
}

/// Regression test for the bug reported in production use: a `~/.ssh/config`
/// with a custom `IdentityFile` under `Host <alias>` only (no `Host *`
/// fallback, no separate block for the resolved `HostName`) must still let
/// auto-bootstrap authenticate, because `bootstrap_and_register` now dials
/// the alias itself rather than the `ssh -G`-resolved `HostName`
/// (`wrapper.rs::BootstrapCandidate::alias`'s docs). Uses
/// `shim_ssh_with_alias_only_bootstrap_config` (no `Host 127.0.0.1` block)
/// instead of `shim_ssh_with_bootstrap_config`.
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_honors_alias_only_identity_file() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env, shim) =
        shim_ssh_with_alias_only_bootstrap_config(tmp.path(), &home, "alias-only-host", mock_sshd_addr, &key_path);

    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let profile_path = profile_path_under(&home, "alias-only-host:22");
    assert!(!profile_path.exists(), "profile must not exist before this test runs");

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("alias-only-host")
        .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // On the native/Windows path, `RusshBackend`'s own SSH host-key TOFU
    // prompt ("Are you sure you want to continue connecting (yes/no)?")
    // fires *before* the app-level "Trust this isekai-helper...? [y/N]"
    // prompt this "y\n" answers — unlike the Unix/`ssh(1)` path, which
    // never asks that first question at all (`StrictHostKeyChecking no` in
    // the shim config suppresses it). See `RusshBackend`'s module docs
    // (`isekai-bootstrap/src/russh_backend.rs`) for why there's no
    // `StrictHostKeyChecking`-equivalent knob to suppress it there too.
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\ny\n" } else { b"y\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") || verbose_log_contains(&verbose_log_path_under(&home), "Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(
        saw_registered,
        "expected wrapper stderr to report trust-store registration (alias-only IdentityFile must still be honored)"
    );
    assert!(profile_path.exists(), "expected profile to be written at {profile_path:?}");
}

/// `#@isekai remote-path` (`isekai-ssh/src/wrapper.rs::resolve_isekai_config`)
/// must actually reach `OpenSshBackend::install_and_start` — the wrapper's own
/// `$HOME/.ssh/config` (not the mock-sshd shim config) is where this
/// directive is parsed from, mirroring how a real user would configure it.
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_honors_remote_path_directive() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    // The `#@isekai remote-path` directive line is appended under the same
    // `Host remote-path-host` block `shim_ssh_with_bootstrap_config` writes
    // to both the shim-only config and `$HOME/.ssh/config` (`wrapper.rs::
    // config_roots` falls back to the latter when no `-F` was passed to
    // `isekai-ssh` itself) — both the wrapper's directive parsing and the
    // native path's own `openssh-config` resolution need to see it.
    let (_bin_dir, path_env, shim) = shim_ssh_with_bootstrap_config(
        tmp.path(),
        &home,
        "remote-path-host",
        mock_sshd_addr,
        &key_path,
        "    #@isekai remote-path ~/custom/isekai-pipe-bin\n",
    );

    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("remote-path-host")
        .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // On the native/Windows path, `RusshBackend`'s own SSH host-key TOFU
    // prompt ("Are you sure you want to continue connecting (yes/no)?")
    // fires *before* the app-level "Trust this isekai-helper...? [y/N]"
    // prompt this "y\n" answers — unlike the Unix/`ssh(1)` path, which
    // never asks that first question at all (`StrictHostKeyChecking no` in
    // the shim config suppresses it). See `RusshBackend`'s module docs
    // (`isekai-bootstrap/src/russh_backend.rs`) for why there's no
    // `StrictHostKeyChecking`-equivalent knob to suppress it there too.
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\ny\n" } else { b"y\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") || verbose_log_contains(&verbose_log_path_under(&home), "Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");

    let uploaded = remote_home.join("custom/isekai-pipe-bin");
    assert!(
        uploaded.exists(),
        "expected the binary to be uploaded at the #@isekai remote-path override {uploaded:?}"
    );
    assert!(
        !remote_home.join(".local/bin").exists(),
        "must not fall back to the default install dir once remote-path is set"
    );
}

/// `#20b`: `#@isekai stun <addr>` must reach `OpenSshBackend::install_and_start`
/// (verified via the stand-in script's own captured argv — it always echoes
/// the same canned `server-reflexive`-bearing report regardless of args, so
/// the argv check is what actually proves the directive was threaded
/// through, not just that the trust store ended up with *some* value), and
/// the resulting `server-reflexive` candidate must land in
/// `HelperTrust.cached_stun_observed_addr`.
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_honors_stun_directive() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env, shim) = shim_ssh_with_bootstrap_config(
        tmp.path(),
        &home,
        "stun-directive-host",
        mock_sshd_addr,
        &key_path,
        "    #@isekai stun 203.0.113.9:3478\n",
    );

    // Stand-in for the real `isekai-pipe serve` process: ignores every arg
    // except recording them for inspection, and always echoes a canned
    // report with a fixed `server-reflexive` candidate (standing in for what
    // a real serve process would report after actually querying
    // `--stun-server`).
    let argv_log = remote_home.join("argv.log");
    let report_with_stun = VALID_BOOTSTRAP_REPORT_JSON.replace(
        r#""candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]"#,
        r#""candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"},{"kind":"server-reflexive","endpoint":"198.51.100.42:56789","source":"stun"}]"#,
    );
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(
        &helper_script_path,
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{report_with_stun}'\n", posix_safe_path(&argv_log)),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("stun-directive-host")
        .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // On the native/Windows path, `RusshBackend`'s own SSH host-key TOFU
    // prompt ("Are you sure you want to continue connecting (yes/no)?")
    // fires *before* the app-level "Trust this isekai-helper...? [y/N]"
    // prompt this "y\n" answers — unlike the Unix/`ssh(1)` path, which
    // never asks that first question at all (`StrictHostKeyChecking no` in
    // the shim config suppresses it). See `RusshBackend`'s module docs
    // (`isekai-bootstrap/src/russh_backend.rs`) for why there's no
    // `StrictHostKeyChecking`-equivalent knob to suppress it there too.
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\ny\n" } else { b"y\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") || verbose_log_contains(&verbose_log_path_under(&home), "Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains("--stun-server 203.0.113.9:3478"),
        "expected #@isekai stun to reach the remote launch command's argv, got: {argv:?}"
    );

    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), "stun-directive-host:22")
        .unwrap()
        .expect("expected a profile for stun-directive-host:22");
    assert_eq!(
        profile.cached_stun_observed_addr.as_deref(),
        Some("198.51.100.42:56789"),
        "the server-reflexive candidate from the handshake should be cached"
    );
}

/// `ISEKAI_PIPE_DESIGN.md` §8 Epic N-3: `#@isekai resume-grace <n>` must reach
/// the remote launch command's argv as `isekai-pipe serve --resume-window
/// <n>` (same technique as `wrapper_auto_bootstrap_honors_stun_directive`) —
/// this is the regression test for the bug where a client-only resume-grace
/// override was silently clamped back down to the server's own unrelated
/// default because it never reached the deploy argv at all.
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_honors_resume_grace_directive() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env, shim) = shim_ssh_with_bootstrap_config(
        tmp.path(),
        &home,
        "resume-grace-directive-host",
        mock_sshd_addr,
        &key_path,
        "    #@isekai resume-grace 42s\n",
    );

    let argv_log = remote_home.join("argv.log");
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(
        &helper_script_path,
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n", posix_safe_path(&argv_log)),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("resume-grace-directive-host")
        .env("HOME", &home)
        // Windows: `default_profiles_dir` checks `LOCALAPPDATA` before `HOME`
        // (see `wrapper_auto_bootstrap_honors_stun_directive`'s comment on the
        // identical env pair for why both are needed).
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // Native/Windows path prompts for SSH host-key TOFU before the app-level
    // "Trust this isekai-helper...?" prompt this "y\n" answers (see
    // `wrapper_auto_bootstrap_honors_stun_directive`'s identical comment).
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\ny\n" } else { b"y\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") || verbose_log_contains(&verbose_log_path_under(&home), "Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains("--resume-window 42"),
        "expected #@isekai resume-grace to reach the remote launch command's argv, got: {argv:?}"
    );
}

/// `ISEKAI_PIPE_DESIGN.md` §8 Epic H: `#@isekai bootstrap-relay addr=... sni=...`
/// must make auto-bootstrap deploy via `LaunchSpec::Relay` (verified via the
/// stand-in script's captured argv, same technique as
/// `wrapper_auto_bootstrap_honors_stun_directive`) instead of the default
/// `LaunchSpec::Direct`, sourcing the relay JWT from `isekai-ssh login`'s
/// saved token file rather than any CLI flag (the wrapper has none), and the
/// resulting profile's cached address must come from the handshake's
/// `relayed` candidate, not `direct-by-bootstrap-host`.
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_honors_bootstrap_relay_directive() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env, shim) = shim_ssh_with_bootstrap_config(
        tmp.path(),
        &home,
        "bootstrap-relay-host",
        mock_sshd_addr,
        &key_path,
        "    #@isekai bootstrap-relay addr=203.0.113.10:443 sni=relay.example.com\n",
    );

    // Pre-seed `isekai-ssh login`'s saved token file — the wrapper has no
    // per-invocation JWT flag (unlike `init --relay-jwt-from-login`), so
    // `bootstrap_and_register` must source it from here unconditionally
    // once `bootstrap-relay` is present. Built via an explicit path (not
    // `FileTokenProvider::from_default_path()`) so this never touches the
    // *test process's own* `$HOME` — only the child `isekai-ssh` process
    // sees `home` via its own `env("HOME", ...)` below.
    let token_path = home.join(".config").join("isekai-ssh").join("token.json");
    isekai_auth::FileTokenProvider::new(token_path).save_relay_jwt("relay-jwt-from-login-store").unwrap();

    let argv_log = remote_home.join("argv.log");
    let report_with_relay = VALID_BOOTSTRAP_REPORT_JSON.replace(
        r#""candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]"#,
        r#""candidates":[{"kind":"relayed","endpoint":"198.51.100.99:45900","source":"relay"}]"#,
    );
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(
        &helper_script_path,
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{report_with_relay}'\n", posix_safe_path(&argv_log)),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("bootstrap-relay-host")
        .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // On the native/Windows path, `RusshBackend`'s own SSH host-key TOFU
    // prompt ("Are you sure you want to continue connecting (yes/no)?")
    // fires *before* the app-level "Trust this isekai-helper...? [y/N]"
    // prompt this "y\n" answers — unlike the Unix/`ssh(1)` path, which
    // never asks that first question at all (`StrictHostKeyChecking no` in
    // the shim config suppresses it). See `RusshBackend`'s module docs
    // (`isekai-bootstrap/src/russh_backend.rs`) for why there's no
    // `StrictHostKeyChecking`-equivalent knob to suppress it there too.
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\ny\n" } else { b"y\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    let mut stderr_text = String::new();
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                stderr_text.push_str(&line);
                if line.contains("Registered") || verbose_log_contains(&verbose_log_path_under(&home), "Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");
    let verbose_log_text = std::fs::read_to_string(verbose_log_path_under(&home)).unwrap_or_default();
    assert!(
        stderr_text.contains("Relay:") || verbose_log_text.contains("Relay:"),
        "expected the trust summary to print the relay address, got stderr: {stderr_text:?}, verbose log: {verbose_log_text:?}"
    );

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(argv.contains("--relay 203.0.113.10:443"), "expected bootstrap-relay's addr to reach the launch command argv, got: {argv:?}");
    assert!(argv.contains("--relay-sni relay.example.com"), "expected bootstrap-relay's sni to reach the launch command argv, got: {argv:?}");
    assert!(!argv.contains("relay-jwt-from-login-store"), "the relay JWT must never appear in argv, got: {argv:?}");

    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), "bootstrap-relay-host:22")
        .unwrap()
        .expect("expected a profile for bootstrap-relay-host:22");
    let legacy_relay = profile.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert_eq!(
        legacy_relay.helper_addr, "198.51.100.99:45900",
        "cached address should come from the handshake's relayed candidate, not direct-by-bootstrap-host"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_writes_nothing_when_confirmation_is_declined() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env, shim) =
        shim_ssh_with_bootstrap_config(tmp.path(), &home, "declined-bootstrap-host", mock_sshd_addr, &key_path, "");

    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let profile_path = profile_path_under(&home, "declined-bootstrap-host:22");

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .args(ssh_shim_args_and_env(&shim).0)
        .envs(ssh_shim_args_and_env(&shim).1)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("declined-bootstrap-host")
        .env("HOME", &home)
        // `isekai_pipe_core::profile::default_profiles_dir` checks
        // `LOCALAPPDATA` *before* `HOME` on Windows (by design — see that
        // function's own doc comment), so `.env("HOME", &home)` alone
        // doesn't redirect the profile directory there the way it does on
        // Unix; it would still resolve against the real CI runner's actual
        // `%LOCALAPPDATA%` (confirmed via a real `test-windows` CI
        // failure). `ISEKAI_PIPE_PROFILES_DIR` is that function's top
        // priority override on every platform, and set to the exact same
        // path its `HOME`-based branch already computes, so this is a
        // no-op on Unix (byte-identical to today's implicit resolution)
        // and the only thing that works on Windows.
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    // See the sibling "y\n" call sites' comment for why Windows needs an
    // extra leading "yes\n" to get past `RusshBackend`'s own host-key TOFU
    // prompt before reaching the app-level confirmation this "n\n" declines.
    let confirm_input: &[u8] = if cfg!(windows) { b"yes\nn\n" } else { b"n\n" };
    child.stdin.take().unwrap().write_all(confirm_input).await.unwrap();

    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("isekai-ssh should not hang after a declined confirmation")
        .expect("failed to wait for isekai-ssh");

    assert!(!output.status.success(), "wrapper must exit non-zero when the user declines the confirmation");
    assert!(output.stdout.is_empty(), "stdout must stay empty, got {:?}", String::from_utf8_lossy(&output.stdout));
    assert!(
        !profile_path.exists(),
        "declining the confirmation must not create a profile file at {profile_path:?}"
    );
}
