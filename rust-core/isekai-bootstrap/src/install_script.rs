//! The remote install/launch script and its response parsing, shared by
//! **both** `BootstrapBackend` implementations (`openssh.rs`'s real `ssh(1)`
//! subprocess and `russh_backend.rs`'s pure-Rust `russh` exec channel).
//!
//! Only two things genuinely differ between those two backends at this
//! layer, and neither is in this module: how an authenticated SSH session is
//! established, and the call that actually pushes bytes over it. Everything
//! else — the embedded `/bin/sh` script uploaded and run on the remote host,
//! the `LaunchSpec`→argv construction, the `dd`/`wc -c` stdin framing, and
//! the stdout-purity response parsing — is one implementation here, called
//! by both.
//!
//! **Why this is shared now, when `russh_backend.rs` originally duplicated
//! it on purpose.** That module's own docs still explain (correctly) why its
//! *credential and host-key plumbing* stays separate from `openssh.rs`:
//! `OpenSshBackend` delegates all of that to `ssh(1)`/`~/.ssh/config`, so
//! there is no common abstraction to share there — only a fake one. That
//! reasoning never extended to a pure string builder and a pure parser, and
//! keeping *those* duplicated turned out to cost exactly what duplication
//! always costs: the macOS `/proc`-guard fix (`795c3192`, "fix: helper再利用の
//! `/proc/<pid>/exe`同一性チェックをmacOSでスキップする") had to be
//! re-applied by hand to the copy that `RusshBackend` later grew, and any
//! *next* fix to this script would have had the same trap waiting. Under the
//! `always-connects.md` principle a silent divergence here is not cosmetic:
//! this script *is* the silent re-deploy mechanism, so one backend quietly
//! missing a fix means that platform's users stop self-healing.
//!
//! The script text below is `openssh.rs`'s, verbatim and unmodified —
//! the already-security-reviewed (#57/#58/#68) production text every real
//! deployment runs today — so unifying moves `RusshBackend` onto the
//! reviewed copy rather than the other way around.
//!
//! What the script does, in order (per invocation, all under one held
//! `flock`):
//!
//! 1. Under a best-effort `flock(1)` on `crate::reuse::lock_file_path`, check
//!    whether `crate::reuse::state_file_path` (scoped by both the binary path
//!    *and* `crate::reuse::launch_fingerprint`, so each distinct topology —
//!    Direct vs. Relay, different relays — tracks its own helper rather than
//!    contending over one) records a still-alive helper (`kill -0`,
//!    `/proc/<pid>/exe` identity check to guard against PID reuse, *and* a
//!    `sha256sum`/`shasum` content check against the binary this exact
//!    invocation would deploy — guards against reusing a helper that's
//!    alive but predates a bugfix to `isekai-pipe serve` itself) — if all
//!    of those match, skip uploading/relaunching entirely and hand back the
//!    recorded handshake (see `crate::reuse`'s module docs for why this is
//!    safe and why `isekai-ssh`'s long-lived-helper model needs it, unlike
//!    `helper_bootstrap.rs`'s intentionally-fresh-every-session Android
//!    path). A stale-content helper is never killed for this — it's simply
//!    not reused, and a fresh one is deployed alongside it (step 2) — the
//!    stale one self-exits later via `--max-idle-lifetime`.
//! 2. Otherwise: `sha256sum` (falling back to `shasum -a 256` on macOS
//!    remotes, which don't ship GNU coreutils) the existing binary (if any,
//!    shared across every topology) and skip re-uploading when it already
//!    matches the expected digest; `base64 -d > ...tmp && chmod 0700 ... &&
//!    mv ...` otherwise, with the base64-encoded binary written to the SSH
//!    session's stdin.
//! 3. Launches `isekai-helper` detached (`setsid` where available — macOS
//!    remotes don't ship a `setsid(1)` binary, so the script falls
//!    back to `trap '' HUP` + `exec` in that case, which is sufficient since
//!    the ssh exec channel never allocates a controlling tty in the first
//!    place; stdin from `/dev/null`, wrapped in a subshell so the ssh exec
//!    channel's direct child exits immediately — see the comment in
//!    `helper_bootstrap.rs` for why that matters) and polls a handshake file
//!    until it's non-empty, then `cat`s
//!    it back over the same exec channel — and, on success, records
//!    `{pid, fingerprint, handshake}` to the state file for a future
//!    invocation to find.
//! 4. Before returning, opportunistically garbage-collects every *other*
//!    fingerprint's state/pid file pair for this same binary whose recorded
//!    pid is no longer alive (`kill -0` fails) — coexisting topologies
//!    (step 1) mean a topology nobody bootstraps anymore would otherwise
//!    leave its small `.state`/`.pid` files behind forever once its helper
//!    process eventually self-exits via `--max-idle-lifetime`. Never touches
//!    a fingerprint whose pid is still alive (that's someone else's
//!    still-active helper, exactly what step 1 exists to protect) or this
//!    invocation's own state file.
//!
//! **stdout purity.** Both backends capture the remote's stdout separately
//! from its stderr and never let either reach this process's own streams
//! (`openssh.rs`'s `Stdio::piped()`, `russh_backend.rs`'s `Data` vs.
//! `ExtendedData` split). [`parse_install_output`] enforces the other half of
//! that contract: anything beyond exactly one non-empty line of handshake
//! JSON is treated as untrusted/corrupted output and rejected
//! (`BootstrapError::UnexpectedStdout`), never heuristically parsed.
//!
//! **Hardening (security review #57/#58/#68)**: both the handshake/log
//! output files *and* the `relay_jwt` file live in a fresh `mktemp -d`
//! directory created per invocation (matching `helper_bootstrap.rs`'s
//! Android bootstrap path exactly — no more fixed
//! `~/.cache/isekai-terminal/helper.{handshake,log}` paths shared across
//! invocations). `relay_sni`/`relay_jwt` are validated against a strict
//! allow-list charset and `relay_sni` is additionally shell-quoted before
//! being interpolated into the remote command string; `relay_jwt` itself
//! never touches argv at all (delivered via `--relay-jwt-file`, exactly like
//! `session_secret` already avoided argv/env for the same reason: other
//! local users on the remote host can read another process's argv via `ps
//! aux`/`/proc/<pid>/cmdline`).

use std::net::SocketAddr;

use base64::Engine as _;

// `ISEKAI_PIPE_INSTALL_DIR`/`ISEKAI_PIPE_BIN_NAME`/`HANDSHAKE_POLL_ATTEMPTS`/
// `HANDSHAKE_POLL_INTERVAL_MS`/`shell_single_quote`/`validate_relay_sni`/
// `validate_relay_jwt` live in `isekai_protocol::bootstrap`, shared with
// `rust-core/src/helper_bootstrap.rs`'s identical constants/helpers (see
// that module's docs for why they must actually be the same literals, not
// just mirrored ones — security review #57/#58 applies to both call sites
// identically).
use isekai_protocol::bootstrap::{
    remote_parent_dir, shell_single_quote, validate_log_level, validate_relay_jwt, validate_relay_sni,
    HANDSHAKE_POLL_ATTEMPTS, HANDSHAKE_POLL_INTERVAL_MS,
};

use crate::client_candidates::fresh_bootstrap_request_v2;
use crate::error::BootstrapError;
use crate::reuse::{launch_fingerprint, lock_file_path, pid_file_path, state_file_path};
use crate::types::LaunchSpec;

/// Emitted by the install script in place of a handshake line when the
/// upload chain (`base64 -d && chmod && mv`) itself fails — distinguished
/// from a bare empty/missing handshake so callers still get
/// `BootstrapError::UploadFailed` (and, via `isekai-bootstrap-plan`'s
/// `classify_bootstrap_error`, `BootstrapFailure::RemoteBinaryMissing`)
/// instead of a generic `HandshakeMissing`.
const UPLOAD_FAILED_MARKER: &str = "ISEKAI_UPLOAD_FAILED";

/// What one remote command run yielded, however it was run: `ssh(1)`'s exit
/// code + piped stdout/stderr (`openssh.rs`), or an SSH exec channel's
/// `exit-status` + `Data`/`ExtendedData` streams (`russh_backend.rs`).
/// `status` is `None` when no exit status ever arrived (a channel that
/// closed or idled out without one).
pub(crate) struct SshOutput {
    pub(crate) status: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// The remote command string plus the stdin bytes it expects, in order.
///
/// The three chunks are kept separate rather than pre-concatenated because
/// `russh_backend.rs` sends them as independent `channel.data()` writes
/// (Codex review finding: the last chunk is the base64-encoded helper
/// binary, potentially tens of MB, and the remote script reads stdin as one
/// continuous byte stream regardless of how many `SSH_MSG_CHANNEL_DATA`
/// packets it arrives in, so materializing the concatenation buys nothing
/// there). `openssh.rs` writes a single pipe and calls
/// [`stdin_payload`](Self::stdin_payload) to flatten them, exactly as it
/// always did.
pub(crate) struct InstallScript {
    /// The `/bin/sh` script to run on the remote host.
    pub(crate) command: String,
    /// `[BootstrapRequestV2 JSON, relay_jwt (empty unless
    /// `LaunchSpec::Relay`), base64-encoded helper binary]`.
    pub(crate) stdin_chunks: [Vec<u8>; 3],
}

impl InstallScript {
    /// Borrowed view of [`stdin_chunks`](Self::stdin_chunks), in order.
    pub(crate) fn stdin_chunk_refs(&self) -> [&[u8]; 3] {
        [self.stdin_chunks[0].as_slice(), self.stdin_chunks[1].as_slice(), self.stdin_chunks[2].as_slice()]
    }

    /// The same bytes flattened into one buffer, for a transport that writes
    /// stdin as a single blob.
    pub(crate) fn stdin_payload(&self) -> Vec<u8> {
        self.stdin_chunks.concat()
    }
}

/// Builds the remote install/launch script and the stdin payload it reads,
/// for either backend.
///
/// The base64-encoded `binary` always travels over stdin regardless of
/// whether the script ends up using it, so the remote script's read position
/// stays aligned across every branch — see the script's own `head -c
/// {encoded_len}` calls, every one of which consumes exactly that many bytes
/// whether it decodes them or discards them.
///
/// Async because `fresh_bootstrap_request_v2` probes the configured STUN
/// servers for this client's own reflexive candidates before the request can
/// be serialized.
pub(crate) async fn build_install_script(
    launch: &LaunchSpec,
    remote_binary_path: &str,
    stun_servers: &[SocketAddr],
    binary: &[u8],
) -> Result<InstallScript, BootstrapError> {
    let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;

    // `#20a`/`#20b`: every bootstrap operation carries a
    // `BootstrapRequestV2` over this same stdin, alongside
    // whatever launch-specific secret (`relay_jwt`) already travels that
    // way. `client_candidates` is now real: one entry per `stun_servers`
    // entry that actually answered (`collect_client_stun_candidates`).
    // `session_id`/`bootstrap_attempt_id` are freshly random per call —
    // see `isekai_protocol::bootstrap_request`'s module docs for why
    // these are their own identifiers, unrelated to any later ATTACH v2
    // fencing identity the eventual QUIC connection will use.
    let bootstrap_request = fresh_bootstrap_request_v2(stun_servers).await;
    let request_bytes = serde_json::to_vec(&bootstrap_request).expect("BootstrapRequestV2 always serializes");
    let request_len = request_bytes.len();

    // `#20b`: pass the first configured STUN server through to the
    // remote `isekai-pipe serve` too (`LaunchSpec::Direct` only —
    // `isekai-pipe serve` itself rejects `--stun-server`/`--relay`
    // together, since they're alternative transports, `#11`'s own
    // research confirmed this validation already exists), so it reports
    // its *own* `server-reflexive` candidate back in the handshake
    // (completing the other half of the exchange —
    // `client_candidates` above is the client's own address(es), this is
    // the server's). Only one is needed server-side (`isekai-pipe serve
    // --stun-server` has always been single-valued, `#11` deliberately
    // scoped multi-STUN collection to the client side only); the
    // remaining configured servers still contribute to
    // `client_candidates` regardless.
    let stun_server_arg = match stun_servers.first() {
        Some(addr) => format!(" --stun-server {addr}"),
        None => String::new(),
    };

    // Per-variant: the `isekai-pipe serve` argv tail, and any extra
    // secret bytes (`relay_jwt` only) that must travel over this same
    // stdin immediately after the `BootstrapRequestV2` JSON.
    let (launch_args, jwt_bytes): (String, Vec<u8>) = match launch {
        LaunchSpec::Relay(relay) => {
            // Security review #57: validate `relay_sni`/`relay_jwt` against a
            // strict allow-list charset *before* interpolating either into a
            // remote shell command string, in addition to shell-quoting
            // `relay_sni` below (defense in depth — a compromised/misconfigured
            // relay or JWT issuer should not be able to smuggle shell
            // metacharacters into either value).
            validate_relay_sni(&relay.relay_sni).map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
            validate_relay_jwt(&relay.relay_jwt).map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
            let remote_log_level = validate_log_level(&relay.remote_log_level)
                .map_err(|e| BootstrapError::InvalidRemoteLogLevel(e.to_string()))?;

            let relay_addr = relay.relay_addr;
            let quoted_sni = shell_single_quote(&relay.relay_sni);
            let idle_lifetime_secs = relay.idle_lifetime_secs;
            let resume_window_secs = relay.resume_window_secs;
            // `#qmux-leg2`: evidence-gated static choice (`ISEKAI_PIPE_DESIGN.md`
            // Epic G/H) — the deployed helper is told once, up front, which
            // transport to use to reach the relay; never a runtime fallback.
            let relay_transport_arg = match relay.relay_transport {
                crate::types::RelayTransportKind::Udp => String::new(),
                crate::types::RelayTransportKind::Qmux => " --relay-transport qmux".to_string(),
            };
            let args = format!(
                "--target 127.0.0.1:22 --relay {relay_addr} --relay-sni {quoted_sni} \
                 --relay-jwt-file $tmpdir/relay_jwt --bootstrap-request-file $tmpdir/bootstrap-request.json\
                 {relay_transport_arg} --max-idle-lifetime {idle_lifetime_secs} \
                 --resume-window {resume_window_secs} --log-level {remote_log_level}"
            );
            (args, relay.relay_jwt.clone().into_bytes())
        }
        // No relay, no STUN: the client dials this host's own SSH
        // bootstrap address at the port reported in `candidates`
        // (`direct-by-bootstrap-host`, `archive/HELPER_PROTOCOL.md` §2).
        // Only the (non-secret-carrying) `BootstrapRequestV2` travels over
        // stdin here — nothing else to deliver out of band.
        LaunchSpec::Direct { idle_lifetime_secs, remote_log_level, remote_bind_port_range, resume_window_secs } => {
            let remote_log_level = validate_log_level(remote_log_level)
                .map_err(|e| BootstrapError::InvalidRemoteLogLevel(e.to_string()))?;
            let bind_port_range_arg = match remote_bind_port_range {
                Some((start, end)) => format!(" --bind-port-range {start}-{end}"),
                None => String::new(),
            };
            let args = format!(
                "--target 127.0.0.1:22 --bind 0.0.0.0:0 --bootstrap-request-file $tmpdir/bootstrap-request.json\
                 {stun_server_arg}{bind_port_range_arg} --max-idle-lifetime {idle_lifetime_secs} \
                 --resume-window {resume_window_secs} --log-level {remote_log_level}"
            );
            (args, Vec::new())
        }
    };

    // Security review #68: use the same per-invocation `mktemp -d` +
    // `trap ... EXIT` pattern as `rust-core/src/helper_bootstrap.rs`
    // (Android bootstrap path) instead of a fixed shared path — see
    // that module's doc comment for the concurrent-session truncation
    // bug a shared fixed path caused. `crate::reuse`'s state/lock/pid
    // files are a *different*, deliberate exception to that principle:
    // they are a shared, per-deployment singleton by design (that's the
    // whole point — one canonical "the currently running helper for
    // this remote-path"), protected by an flock instead of by being
    // freshly named per invocation.
    //
    // Security review #58: `relay_jwt` (the MASQUE relay bearer token)
    // is written to `$tmpdir/relay_jwt` via this SSH session's
    // stdin rather than embedded in the command line, then passed to
    // isekai-helper as `--relay-jwt-file` — argv would otherwise be
    // readable by any other local user on the remote host via `ps
    // aux`/`/proc/<pid>/cmdline`, exactly like `session_secret` already
    // avoids that path.
    //
    // `#20a-2`: the `BootstrapRequestV2` JSON travels first on this same
    // stdin, immediately followed by `relay_jwt` (if any) and then the
    // base64-encoded binary — all length-prefixed (the lengths
    // themselves aren't secret, so they're safe to interpolate into the
    // command string). The request/jwt pieces are split off with
    // `dd bs=1 count=N` rather than `head -c N`: confirmed via a real
    // `test-macos` CI failure that macOS's `head -c` (unlike GNU's) reads
    // through its own stdio buffer when the input is a pipe, so it can
    // silently consume *more* than the requested N bytes from stdin —
    // swallowing the immediately-following `relay_jwt` bytes into a
    // buffer that's discarded once `head` exits, leaving the next read
    // with 0 bytes. `dd bs=1` always issues exactly N single-byte reads,
    // so it can never over-consume; this only matters for these two
    // small, bounded-size pieces with more stdin data after them — the
    // final (and only large, multi-MB) piece, the base64-encoded binary,
    // is still read with `head -c`/`base64 -d` directly since being the
    // last thing on stdin, over-reading there has no observable effect.
    // The request/jwt byte counts are verified with `wc -c` (piped
    // through `tr -d '[:space:]'` first — macOS's `wc -c` right-justifies
    // its count with leading spaces even for a single stdin stream, e.g.
    // `     136`, which made the `-eq` comparison itself unreliable
    // there too, independently of the `head`-over-read issue above) so a
    // truncated stdin (e.g. the ssh connection dropping mid-write) fails
    // closed instead of launching `isekai-pipe serve` against a
    // partially-written file.
    let jwt_len = jwt_bytes.len();
    let read_jwt_step = if jwt_len > 0 {
        format!(
            "dd bs=1 count={jwt_len} > $tmpdir/relay_jwt 2>/dev/null && [ \"$(wc -c < $tmpdir/relay_jwt | tr -d '[:space:]')\" -eq {jwt_len} ] && "
        )
    } else {
        String::new()
    };

    let remote_dir = remote_parent_dir(remote_binary_path);
    let fingerprint = launch_fingerprint(launch);
    // `lock_path` stays scoped by `remote_binary_path` alone (shared
    // across every topology — it guards the upload step below, which is
    // a shared resource); `state_path`/`pid_path` are scoped by
    // `fingerprint` too, so a different topology's still-alive helper is
    // simply a different pair of files, never something this bootstrap
    // needs to detect-and-kill (`crate::reuse`'s module docs).
    let lock_path = lock_file_path(remote_binary_path);
    let state_path = state_file_path(remote_binary_path, &fingerprint);
    let pid_path = pid_file_path(remote_binary_path, &fingerprint);
    // `remote_binary_path`/`pid_path` writes below go through a `.$$`
    // (this script's own shell pid, always unique per invocation)-suffixed
    // scratch path followed by an atomic same-directory `mv` into the final
    // fixed name — the same pattern `state_path` already used. Without this,
    // two concurrent bootstraps of the same host (realistic: a second client
    // dialing in while the first is still mid-upload) racing past the
    // best-effort `flock` below — either because it times out after 30s
    // (never made unconditional-fail, see the comment above the `flock`
    // invocation itself) or because the remote has no `flock(1)` at all —
    // would both write through the *same* fixed `.tmp`/pid path, corrupting
    // each other's in-flight upload or silently overwriting each other's
    // recorded launch pid. `pid_path` itself must still end up at its fixed,
    // persistent name once launch succeeds (not e.g. inside `$tmpdir`,
    // which is `rm -rf`'d on this script's own exit) — a *later* invocation's
    // GC pass (below) discovers and reaps it by that exact fixed name once
    // its process dies (`crate::reuse::pid_file_path`'s own docs, and
    // `openssh_e2e.rs`'s GC test asserts on this exact path surviving).
    // On the failure branches (`upload_ok=0`, handshake never appears) the
    // `.$$`-suffixed scratch file is `rm -f`'d explicitly, since it lives
    // outside `$tmpdir` and nothing else would ever clean it up.
    let expected_sha256 = isekai_trust::hex_sha256(binary);
    let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
    let encoded_len = encoded.len();
    let upload_failed_marker = UPLOAD_FAILED_MARKER;

    // `9>&-` right before `setsid` below matters: a plain `setsid cmd &`
    // would otherwise inherit this shell's fd 9 (the `flock` below) into
    // the detached, long-lived grandchild, which then holds that same
    // open file description (and therefore the lock) open for its
    // entire lifetime — a *second* invocation's own `flock -w 30 9`
    // would then block for the full 30s waiting on a lock the first
    // helper process never releases, found via a real hang in
    // `openssh.rs`'s own e2e tests before this fix.
    let command = format!(
        r#"umask 077
mkdir -p {remote_dir} 2>/dev/null
exec 9>>{lock_path} 2>/dev/null
if command -v flock >/dev/null 2>&1; then flock -w 30 9 2>/dev/null || true; fi
sha256_of() {{
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" 2>/dev/null | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1
  fi
}}
tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT
if dd bs=1 count={request_len} > $tmpdir/bootstrap-request.json 2>/dev/null && [ "$(wc -c < $tmpdir/bootstrap-request.json | tr -d '[:space:]')" -eq {request_len} ] && {read_jwt_step}true; then
  reuse_envelope=""
  if [ -f {state_path} ]; then
    existing_pid=$(sed -n '1p' {state_path} | cut -d' ' -f1)
    existing_fp=$(sed -n '1p' {state_path} | cut -d' ' -f2)
    if [ -n "$existing_pid" ] && kill -0 "$existing_pid" 2>/dev/null; then
      # `/proc/<pid>/exe` doesn't exist on macOS remotes (no /proc at all) —
      # skip this extra identity check there and trust `kill -0` + fingerprint
      # match alone (confirmed via a real `test-macos` CI failure: without
      # this `-d /proc` guard, `existing_exe` was always empty on macOS, so
      # the still-alive helper was never reused, defeating the whole point
      # of this reuse path there). Safe to skip: per the comment below,
      # `existing_fp` already pins this state file to this exact fingerprint,
      # so this check was already "defense-in-depth, not a decision point".
      if [ -d /proc ]; then
        existing_exe=$(readlink -f /proc/$existing_pid/exe 2>/dev/null)
        expected_exe=$(readlink -f {remote_binary_path} 2>/dev/null)
      else
        existing_exe=ok
        expected_exe=ok
      fi
      # `existing_fp` should always equal `{fingerprint}` here (the file
      # itself is fingerprint-scoped) — kept as a cheap defense-in-depth
      # sanity check, not a decision point: this bootstrap never touches a
      # *different* topology's state/pid file, so there is no "stale, kill
      # it" case to handle here at all (`crate::reuse`'s module docs).
      #
      # `existing_sha` guards against reusing a still-alive helper that
      # predates a bugfix to `isekai-pipe serve` itself: pid/exe-path/
      # fingerprint all matching only proves the *same binary path* is
      # still running, not that its *contents* are still what this
      # `isekai-ssh` build expects. A stale-but-alive process is never
      # killed here (same reasoning as the fingerprint-mismatch case
      # above: some other still-active client may be mid-session on it) —
      # this just falls through to the normal upload+launch path below,
      # which deploys and starts a fresh helper the stale one doesn't
      # interfere with. The stale one self-exits via `--max-idle-lifetime`.
      if [ -n "$existing_exe" ] && [ "$existing_exe" = "$expected_exe" ] && [ "$existing_fp" = "{fingerprint}" ]; then
        existing_sha=$(sha256_of {remote_binary_path})
        if [ "$existing_sha" = "{expected_sha256}" ]; then
          reuse_envelope=$(sed -n '2p' {state_path})
        fi
      fi
    fi
  fi
  if [ -n "$reuse_envelope" ]; then
    head -c {encoded_len} > /dev/null
    printf '%s\n' "$reuse_envelope"
  else
    need_upload=1
    if [ -x {remote_binary_path} ]; then
      current_sha=$(sha256_of {remote_binary_path})
      [ -n "$current_sha" ] && [ "$current_sha" = "{expected_sha256}" ] && need_upload=0
    fi
    upload_ok=1
    if [ "$need_upload" -eq 1 ]; then
      head -c {encoded_len} | base64 -d > {remote_binary_path}.tmp.$$ && chmod 0700 {remote_binary_path}.tmp.$$ && mv {remote_binary_path}.tmp.$$ {remote_binary_path} || {{ rm -f {remote_binary_path}.tmp.$$ 2>/dev/null; upload_ok=0; }}
    else
      head -c {encoded_len} > /dev/null
    fi
    if [ "$upload_ok" -eq 0 ]; then
      echo {upload_failed_marker}
    else
      if command -v setsid >/dev/null 2>&1; then
        ( setsid {remote_binary_path} serve {launch_args} </dev/null >$tmpdir/handshake 2>$tmpdir/log 9>&- & echo $! > {pid_path}.$$ )
      else
        ( ( trap '' HUP; exec {remote_binary_path} serve {launch_args} </dev/null >$tmpdir/handshake 2>$tmpdir/log 9>&- ) & echo $! > {pid_path}.$$ )
      fi
      for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do
        [ -s $tmpdir/handshake ] && break
        sleep {sleep_secs}
      done
      if [ -s $tmpdir/handshake ]; then
        envelope=$(cat $tmpdir/handshake)
        new_pid=$(cat {pid_path}.$$ 2>/dev/null)
        mv {pid_path}.$$ {pid_path} 2>/dev/null
        ( printf '%s %s\n' "$new_pid" "{fingerprint}"; printf '%s\n' "$envelope" ) > {state_path}.tmp.$$ && mv {state_path}.tmp.$$ {state_path}
        printf '%s\n' "$envelope"
      else
        rm -f {pid_path}.$$ 2>/dev/null
      fi
    fi
  fi
  for gc_state in {remote_binary_path}.*.state; do
    [ -e "$gc_state" ] || continue
    [ "$gc_state" = {state_path} ] && continue
    gc_pid=$(sed -n '1p' "$gc_state" | cut -d' ' -f1)
    if [ -z "$gc_pid" ] || ! kill -0 "$gc_pid" 2>/dev/null; then
      rm -f "$gc_state" "${{gc_state%.state}}.pid"
    fi
  done
fi
"#
    );

    Ok(InstallScript { command, stdin_chunks: [request_bytes, jwt_bytes, encoded.into_bytes()] })
}

/// Parses what the install script printed on stdout back into the
/// `HandshakeJson` `isekai-pipe serve` reported (`archive/HELPER_PROTOCOL.md`
/// §2), enforcing the stdout-purity contract described in this module's docs.
pub(crate) fn parse_install_output(out: &SshOutput) -> Result<isekai_protocol::HandshakeJson, BootstrapError> {
    let non_empty_lines: Vec<&[u8]> = out.stdout.split(|&b| b == b'\n').filter(|line| !line.is_empty()).collect();

    match non_empty_lines.as_slice() {
        [] => Err(BootstrapError::HandshakeMissing {
            status: out.status,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        [marker] if *marker == UPLOAD_FAILED_MARKER.as_bytes() => Err(BootstrapError::UploadFailed {
            status: out.status,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        // `#20a-4`: every launch above sends a `BootstrapRequestV2`, so a
        // compliant `isekai-pipe serve` always echoes back a
        // `BootstrapReportV2` envelope (never a bare `HandshakeJson`) —
        // decode accordingly and unwrap the inner handshake. The reuse
        // path replays a *previously* captured envelope verbatim
        // (including its now-stale `session_id`/`bootstrap_attempt_id`)
        // rather than one matching *this* invocation's own
        // `bootstrap_request` — safe because no code path here or in
        // any caller ever compares those echoed ids against the request
        // that produced them (they exist for other correlation
        // purposes, per `isekai_protocol::bootstrap_request`'s module
        // docs); only `.handshake` is ever consulted.
        [single] => Ok(isekai_protocol::bootstrap_request::decode_bootstrap_report_v2(single)?.handshake),
        _ => Err(BootstrapError::UnexpectedStdout { extra_lines: non_empty_lines.len() - 1 }),
    }
}

/// Turns a `uname -m` probe's [`SshOutput`] into the normalized architecture
/// string both backends' `detect_remote_arch` return — the command failing
/// outright is a `RemoteCommandFailed`, an unrecognized architecture an
/// `UnsupportedArch`.
pub(crate) fn parse_uname_output(out: &SshOutput) -> Result<String, BootstrapError> {
    if out.status != Some(0) {
        return Err(BootstrapError::RemoteCommandFailed {
            command: "uname -m".to_string(),
            status: out.status,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    normalize_uname_arch(&String::from_utf8_lossy(&out.stdout))
}

/// Normalizes `uname -m`'s output to `"x86_64"`/`"aarch64"`, or rejects it —
/// same mapping as `rust-core/src/helper_bootstrap.rs`'s
/// `IsekaiPipeBinaries::select_for` (Android's own remote-bootstrap path),
/// kept identical deliberately rather than reinvented here.
fn normalize_uname_arch(uname_m: &str) -> Result<String, BootstrapError> {
    match uname_m.trim() {
        "x86_64" => Ok("x86_64".to_string()),
        "aarch64" | "arm64" => Ok("aarch64".to_string()),
        other => Err(BootstrapError::UnsupportedArch(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(status: Option<i32>, stdout: &str) -> SshOutput {
        SshOutput { status, stdout: stdout.as_bytes().to_vec(), stderr: Vec::new() }
    }

    #[test]
    fn normalize_uname_arch_accepts_x86_64() {
        assert_eq!(normalize_uname_arch("x86_64\n").unwrap(), "x86_64");
    }

    #[test]
    fn normalize_uname_arch_accepts_aarch64_and_arm64_aliases() {
        assert_eq!(normalize_uname_arch("aarch64\n").unwrap(), "aarch64");
        assert_eq!(normalize_uname_arch("arm64\n").unwrap(), "aarch64");
    }

    #[test]
    fn normalize_uname_arch_rejects_unknown_architectures() {
        let err = normalize_uname_arch("riscv64\n").unwrap_err();
        assert!(matches!(err, BootstrapError::UnsupportedArch(ref a) if a == "riscv64"));
    }

    #[test]
    fn parse_uname_output_rejects_a_failed_probe() {
        let err = parse_uname_output(&out(Some(127), "")).unwrap_err();
        assert!(matches!(err, BootstrapError::RemoteCommandFailed { ref command, .. } if command == "uname -m"));
    }

    #[test]
    fn parse_install_output_reports_missing_handshake_for_empty_stdout() {
        let err = parse_install_output(&out(Some(0), "\n\n")).unwrap_err();
        assert!(matches!(err, BootstrapError::HandshakeMissing { .. }));
    }

    #[test]
    fn parse_install_output_maps_the_upload_marker_to_upload_failed() {
        let err = parse_install_output(&out(Some(0), "ISEKAI_UPLOAD_FAILED\n")).unwrap_err();
        assert!(matches!(err, BootstrapError::UploadFailed { .. }));
    }

    #[test]
    fn parse_install_output_rejects_extra_stdout_lines() {
        let err = parse_install_output(&out(Some(0), "motd noise\n{}\n")).unwrap_err();
        assert!(matches!(err, BootstrapError::UnexpectedStdout { extra_lines: 1 }));
    }
}
