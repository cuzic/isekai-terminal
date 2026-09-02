//! Detects that this process's local peer — normally `ssh(1)`'s
//! `ProxyCommand`, which owns the other end of our stdin/stdout — is gone,
//! even when nothing is currently being read or written. Task 2.9 / issue
//! #111 (`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` Round 6/7): `ssh(1)` does
//! not reliably reap this process when it is killed externally
//! (`SIGTERM`/`SIGKILL` — confirmed by direct experiment, 2026-09-02), so
//! without an active signal, an orphaned `isekai-pipe connect` can spend up
//! to the full server-granted resume window (10 days by default on the
//! relay route) redialing and replaying bytes into a stdout nobody reads.
//!
//! `resume_loop::PumpFailure`'s `Local`/`Remote` split (and the EOF-latch
//! next to it) catch this reactively, as a side effect of an actual read or
//! write attempt — but a quiet session (nothing flowing either direction)
//! never attempts either, so those mechanisms can't fire while this process
//! sits blocked in `resume_with_backoff_until_deadline` or an initial
//! dial/handshake. This module closes that gap directly, instead of
//! extending the reactive approach further.
//!
//! ## Why `poll()`, not `prctl(PR_SET_PDEATHSIG)`/`kqueue`
//!
//! An earlier design considered forcing `isekai-pipe connect` to become
//! `ssh(1)`'s literal direct child (prepending `exec` to the `ProxyCommand`
//! string) so a kernel parent-death primitive (`PR_SET_PDEATHSIG` on Linux,
//! `kqueue`/`EVFILT_PROC`/`NOTE_EXIT` on macOS) could be armed against it.
//! Adversarial review (2 rounds, 2 independent reviewers, with direct
//! experiments run by both) found:
//!
//! - `ssh(1)` **already** execs `ProxyCommand` via `$SHELL -c "exec <cmd>"`
//!   — `ssh_config(5)`: "the command string ... is executed using the
//!   user's shell 'exec' directive to avoid a lingering shell process".
//!   `isekai-pipe connect` is already `ssh(1)`'s direct, PID-stable child
//!   today, with no wrapper change needed. Prepending a *second* `exec` (as
//!   the rejected design proposed) produces `exec exec <cmd>`, which every
//!   POSIX shell tested (`dash`, `bash`) fails immediately with
//!   `exec: exec: not found` (`exec` is a special builtin, so the outer
//!   `exec` does a `PATH` lookup for a program literally named `exec`) —
//!   this would have broken every Unix connection. See
//!   `isekai-ssh/src/wrapper.rs::proxy_command`'s doc comment for the
//!   corresponding one-line invariant this leaves behind (the command
//!   string must stay a single simple command, or the implicit `exec`
//!   guarantee is lost).
//! - Even with that premise corrected, `PR_SET_PDEATHSIG`/`kqueue` would
//!   still need: two divergent platform-specific implementations (Linux vs.
//!   macOS, the latter unverified in this repo's CI at review time); an
//!   arm-time race (the parent can already be dead before the primitive is
//!   armed) that needs an explicit guard — and a real, independently-found
//!   bug in the obvious form of that guard: an orphan reparents to the
//!   nearest **child subreaper** (e.g. `systemd --user`), not PID 1, so
//!   `getppid() == 1` silently never fires; and `SIGTERM`'s default action
//!   terminates abruptly, skipping every graceful-shutdown `Drop` — which
//!   reintroduces exactly the problem `run_resume_loop`'s explicit
//!   `quic_write.reset(0)` vs. clean-FIN distinction exists to avoid (an
//!   abrupt kill here would make the *server* park the session for the
//!   full resume grace instead of tearing it down immediately).
//!
//! A `poll()`-based watchdog sidesteps all of this: it's level-triggered, so
//! there's no arm-time race to guard in the first place (if the peer is
//! already gone when `poll()` is called, it returns immediately, no
//! snapshot/comparison needed); it tests the actual predicate this process
//! cares about — "is anyone still on the other end of my stdio" — rather
//! than a proxy for it ("did one specific PID exit"), so it also catches
//! the rarer case where `ssh(1)` itself is still alive but this specific
//! pipe broke; and firing it wakes ordinary async code via a channel, which
//! can run a normal graceful shutdown instead of fighting
//! async-signal-safety inside a signal handler.
//!
//! The per-fd `events`/sleep-guard fix below is *intended* to restore
//! exactly the "one identical implementation for Linux, macOS, and every
//! BSD" that motivated choosing `poll()` over `prctl`/`kqueue` in the first
//! place — a claim that briefly looked false when real
//! `rust-core-test-macos` CI (2026-09-02) found the *first* draft's
//! `events: 0` didn't work on Darwin, and a Darwin-specific `kqueue` path
//! (`EVFILT_READ`/`EVFILT_WRITE` + `EV_CLEAR`) was designed as the fix.
//! Both adversarial reviewers who'd proposed that design independently
//! withdrew it once they found the simpler per-fd-`events`-plus-sleep-guard
//! fix instead (see the next section), on the reasoning that it closes the
//! same gap without adding platform-specific code.
//!
//! **That reasoning is not yet confirmed by measurement** — this module's
//! own established pattern is not to assert something CI can settle as fact
//! ahead of CI actually settling it (this exact paragraph would otherwise
//! be a fifth instance of that mistake in this file's history). Whether
//! `rust-core-test-macos` is green on the commit containing this fix is the
//! actual evidence; check the PR/commit history rather than trusting this
//! sentence's confidence. If it's red with both new two-fd tests failing,
//! the per-`events` fix didn't take on Darwin at all and the withdrawn
//! `kqueue` design should be revisited; if only one direction fails, see
//! that test's own doc comment for what it implies and why keeping both fds
//! already protects production either way.
//!
//! ## Why fd 0 *and* fd 1, why per-fd `events`, and why a sleep-guard
//!
//! `ssh(1)` gives its `ProxyCommand` two independent `pipe(2)`s (confirmed
//! by direct experiment, 2026-09-02: different inodes, not a shared
//! `socketpair(2)` — an earlier draft of this analysis wrongly assumed the
//! latter, generalizing from `ssh_proxy_fdpass_connect()`'s `ProxyUseFdpass`
//! path instead of the ordinary `ProxyCommand` path actually used here).
//! Watching both removes any dependency on which specific flag a given
//! platform's `poll()` happens to report for which half:
//!
//! - fd 0 (our stdin, `ssh(1)`'s write end): reports `POLLHUP` once `ssh(1)`
//!   closes its end (measured on Linux — Darwin routes this through
//!   `EVFILT_READ`'s `EV_EOF`, which its `poll()` shim may map to a
//!   different bit; `TERMINAL` accepts either, so this is a difference in
//!   which flag fires, not in whether detection works).
//! - fd 1 (our stdout, `ssh(1)`'s read end): reports `POLLERR` (not
//!   `POLLHUP`) once `ssh(1)` closes its end on Linux — measured directly; a
//!   watchdog that only checked `POLLHUP` would silently never fire on this
//!   fd. Same Darwin caveat as fd 0 above.
//!
//! An earlier draft requested `events: 0` on both, reasoning that
//! `POLLERR`/`POLLHUP`/`POLLNVAL` are always reported in `revents`
//! regardless of the requested mask. That's true on Linux, but real
//! `rust-core-test-macos` CI (2026-09-02) found it false on Darwin: Darwin
//! does not implement `poll(2)` natively — XNU's `poll_nocancel()`
//! translates each `pollfd` into `kqueue` registrations, registering
//! `EVFILT_READ` only if `events` asks for a read direction and
//! `EVFILT_WRITE` only if it asks for a write direction. With `events: 0`,
//! *neither* filter is registered, so that fd has nothing watching it and
//! can never produce a `revents` at all — exactly the observed symptom
//! (both directions timed out, not one).
//!
//! So each fd now requests the direction it actually supports: `POLLIN` for
//! fd 0, `POLLOUT` for fd 1. Never the other way around — asking Darwin to
//! register a read filter on fd 1 (a pipe *write* end) is asking for
//! something that fd cannot do, which is **suspected** (never measured —
//! the code deliberately avoids this configuration rather than
//! characterizing what it actually does, matching this module's own history
//! of inferences-stated-as-fact turning out wrong) to produce `POLLNVAL`
//! (in `TERMINAL`) rather than being silently ignored, which would make the
//! watchdog fire immediately and unconditionally at startup rather than
//! only on a real close.
//!
//! Requesting a "real" direction reintroduces the spurious-wakeup problem
//! the `events: 0` draft existed to avoid, from **both** sides now, not just
//! `POLLOUT`-on-fd-1: `poll()` is level-triggered and this thread never
//! actually reads fd 0 (`resume_loop::pump_c2h` does, on a different
//! thread), so `ssh(1)`'s own version banner sitting unread through the
//! entire dial/handshake/`retry_while_busy_other_session` window — or any
//! trzsz transfer's sustained inbound flow — would otherwise make `poll()`
//! return `POLLIN` immediately, over and over, a hot 100%-CPU spin instead
//! of a block (found by adversarial review, 2026-09-02, before it ever
//! reached CI). The fix is the same for both directions: whenever `poll()`
//! returns with no `TERMINAL` bit set, sleep [`SPURIOUS_BACKOFF`] before
//! polling again, converting a spurious readiness of *either* kind into a
//! bounded interval-poll at zero CPU between checks, rather than trying to
//! special-case each direction's own spurious-wakeup source separately.
//! [`SPURIOUS_BACKOFF`]'s length is a non-issue against the multi-day
//! orphan window this whole module exists to bound.
//!
//! **This means the loop is not actually "blocking with zero CPU until
//! `ssh(1)` dies" in steady state** (an earlier draft of this doc claimed
//! exactly that, before `POLLOUT` was added — flagged as an overclaim by
//! adversarial review, 2026-09-02): fd 1's `POLLOUT` is ready essentially
//! continuously on a healthy pipe write end, so this thread spends most of
//! its life in the sleep-and-repoll cycle, not parked in a single indefinite
//! `poll()` call. Detection latency for a genuine close is therefore bounded
//! by [`SPURIOUS_BACKOFF`] (at most one interval, since `poll()` returning
//! `TERMINAL` short-circuits the sleep entirely) rather than being
//! instantaneous — a real, deliberate tradeoff, not a bug, made because the
//! alternative (only watching directions that are provably idle when
//! healthy) would mean not watching fd 1 for its `POLLERR` case at all.
//!
//! ## Why a dedicated thread, not `tokio::io::unix::AsyncFd`/`mio`
//!
//! Rust does have portable epoll/kqueue/IOCP abstractions (`mio`, and
//! `tokio::io::unix::AsyncFd` built on it) — this module isn't hand-rolling
//! `poll()` because those don't exist, but because they all require making
//! the underlying fd `O_NONBLOCK` to register it for readiness
//! notification, and that's a property of the *open file description*, not
//! something scoped to just this module's own usage. Fd 1 isn't exclusively
//! this module's to configure: `resume_loop::pump_h2c` does real, blocking
//! writes to that same fd for the actual data plane, so registering it with
//! `AsyncFd` would silently make those writes non-blocking too — surfacing
//! a transient `WouldBlock` as a spurious `PumpFailure::Local` and exiting a
//! healthy session on an ordinary partial write. A plain blocking
//! `std::thread` calling raw `poll(2)` avoids the question entirely, at the
//! cost of exactly the raw-syscall platform quirks (the Darwin `events: 0`
//! gap above) a library like `mio` would otherwise have already solved.

use tokio::sync::watch;

/// Fires exactly once, when this process's local peer is gone. Clone
/// freely — every clone observes the same one-shot transition via
/// [`watch::Receiver::changed`]. On non-Unix targets (see [`spawn`]) this
/// simply never fires; callers must not treat that as a guarantee the peer
/// is alive, only as "no additional signal is available on this platform"
/// (the `resume_loop` EOF-latch is the fallback there).
pub(crate) type ParentGoneWatch = watch::Receiver<bool>;

/// Waits for the watchdog to fire — resolves once `*rx.borrow() == true`,
/// never before.
///
/// Deliberately **not** `let _ = rx.changed().await;` (an earlier, wrong
/// draft — found by adversarial review, 2026-09-02): `changed()` also
/// returns (as `Err`) the instant every sender is dropped, which happens on
/// two reachable paths that mean the *opposite* of "the parent is gone" —
/// `spawn`'s watchdog thread failing to start at all (fd/memory exhaustion),
/// and `watch_loop`'s own `poll()` call failing unexpectedly and giving up.
/// Both drop `tx` without ever sending `true`. Treating that the same as a
/// real fire made `connect_command`'s `select!` abort the connection
/// immediately in the first case, and abort a healthy mid-session pump in
/// the second — exactly backwards from those paths' own "fail open, fall
/// back to reactive detection" comments. Looping on `changed()` and checking
/// the actual value, then pending forever once the channel closes without
/// ever having fired, fixes both: a closed-without-firing channel now
/// correctly means "no additional signal available," not "parent gone."
pub(crate) async fn wait(rx: &mut ParentGoneWatch) {
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await
}

#[cfg(unix)]
pub(crate) fn spawn() -> ParentGoneWatch {
    let (tx, rx) = watch::channel(false);
    // Leak a clone of `tx` so the channel can never close *without* firing
    // — both of `watch_loop`'s own give-up paths below (an unexpected
    // `poll()` error) and this function's own thread-spawn-failure path
    // drop their copy of `tx` without ever sending `true`, meaning they are
    // meant to fail *open* ("no additional signal available on this run,
    // defer to the reactive fallback") — but `wait()` cannot otherwise tell
    // that apart from a real fire, since `changed()` also returns once
    // every sender is gone (found by adversarial review, 2026-09-02, N1 /
    // "Blocking 2": both paths' own comments already said "fail open," but
    // the actual effect was the opposite — an immediate or spurious
    // mid-session fire). This leaked clone is exactly the same trick the
    // non-Unix `spawn` below already used for the same reason.
    std::mem::forget(tx.clone());
    // Deliberately not joined/cancelled anywhere: the process either exits
    // normally (the OS tears down every thread, this one included) or this
    // thread itself drives that exit by sending on `tx` — there is no
    // third case where anything still needs to wait on it.
    let spawned = std::thread::Builder::new()
        .name("parent-watchdog".to_string())
        .spawn(move || watch_loop(tx, &[(0, libc::POLLIN), (1, libc::POLLOUT)]));
    if spawned.is_err() {
        log::warn!("isekai-pipe connect: failed to spawn the parent-liveness watchdog thread; falling back to reactive detection only");
    }
    rx
}

/// How long `watch_loop` sleeps after a `poll()` that returned with no
/// `TERMINAL` bit set (i.e. the fd merely became readable/writable, not
/// closed) before polling again — see the module doc's "why per-fd `events`,
/// and why a sleep-guard" section for why this exists at all. Irrelevant
/// against the multi-day orphan window this module exists to bound.
const SPURIOUS_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// The actual `poll()` loop, over `fds` — each entry is `(fd, events)`;
/// production always passes `&[(0, POLLIN), (1, POLLOUT)]` (this process's
/// own stdin/stdout, each requesting the direction it actually supports —
/// see the module doc); tests pass a single throwaway pipe fd instead, so
/// they exercise this exact function (its `TERMINAL` mask, its `EINTR`
/// retry, its give-up path, its `SPURIOUS_BACKOFF`) rather than a
/// hand-copied stand-in that could silently drift from it. An earlier draft
/// of this module's own test did exactly that — reimplemented this loop
/// inline instead of calling it — and a round of adversarial review flagged
/// it as the same "test pins a copy, not the real function" mistake
/// `resume_loop.rs` has already made and fixed once before
/// (`effective_resume_window`, round 2 review of Epic R PR3).
#[cfg(unix)]
fn watch_loop(tx: watch::Sender<bool>, fds: &[(libc::c_int, libc::c_short)]) {
    const TERMINAL: libc::c_short = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
    let mut pollfds: Vec<libc::pollfd> = fds.iter().map(|&(fd, events)| libc::pollfd { fd, events, revents: 0 }).collect();
    loop {
        for pfd in &mut pollfds {
            pfd.revents = 0;
        }
        // SAFETY: `pollfds` is a valid, correctly-sized buffer for the
        // duration of this call; `poll(2)` only reads/writes through the
        // pointer we give it.
        let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // An unexpected `poll()` failure leaves this watchdog unable to
            // usefully continue; give up quietly rather than busy-loop on a
            // condition that keeps recurring. Fails open — see `spawn`'s
            // leaked extra sender clone for why dropping `tx` here does not
            // by itself make `wait()` treat this as a fire.
            log::warn!("isekai-pipe connect: parent-liveness watchdog poll() failed unexpectedly, giving up: {err}");
            return;
        }
        if pollfds.iter().any(|pfd| pfd.revents & TERMINAL != 0) {
            let _ = tx.send(true);
            return;
        }
        // `ret > 0` here means some fd became ready in the direction we
        // asked for (`POLLIN` data pending on fd 0, `POLLOUT` space
        // available on fd 1) without anything terminal — spurious from this
        // module's point of view. Sleeping instead of immediately polling
        // again is what keeps this level-triggered loop from spinning; see
        // the module doc.
        std::thread::sleep(SPURIOUS_BACKOFF);
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn() -> ParentGoneWatch {
    // No `poll()`-equivalent this module implements on non-Unix targets
    // today (see module docs) — dropping `tx` immediately closes the
    // channel, which `wait` now correctly treats as "no signal available,
    // never fires" rather than a real fire (see `wait`'s docs, N1). This is
    // the MSYS2/Cygwin-hosted `ssh.exe` case in practice: the spawned
    // `isekai-pipe connect` there is a native Windows binary with no
    // `poll(2)`, so `resume_loop`'s EOF-latch is the only signal available.
    let (_tx, rx) = watch::channel(false);
    rx
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn pipe_pair() -> (libc::c_int, libc::c_int) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    /// Comfortably longer than [`SPURIOUS_BACKOFF`] (currently 1s) — the
    /// negative half of [`assert_watch_loop_stays_quiet_then_fires`] must
    /// outlast several full sleep-and-repoll cycles, not just the first one,
    /// or it would pass vacuously (still inside the very first sleep) without
    /// actually proving the loop keeps declining to fire in steady state
    /// (an earlier draft used 300ms — shorter than `SPURIOUS_BACKOFF`
    /// itself — flagged as proving nothing by adversarial review, 2026-09-02).
    const QUIET_WINDOW: std::time::Duration = std::time::Duration::from_millis(3_500);

    /// Runs the real `watch_loop` (not a hand-copied stand-in) on `watched`,
    /// requesting `events`, in a background thread. First asserts it stays
    /// quiet for [`QUIET_WINDOW`] while the peer is still alive (this is the
    /// negative half — without it, a `watch_loop` that fired *unconditionally*
    /// at startup, e.g. because Darwin turns an unsupported direction into an
    /// immediate `POLLNVAL`, would make this test pass for the wrong reason;
    /// found necessary by adversarial review, 2026-09-02, after the
    /// `events: POLLIN`-on-fd-1 near-miss below), then calls `close_peer` and
    /// asserts it fires within a generous timeout.
    fn assert_watch_loop_stays_quiet_then_fires(watched: libc::c_int, events: libc::c_short, close_peer: impl FnOnce() + Send + 'static) {
        let (tx, mut rx) = watch::channel(false);
        let handle = std::thread::spawn(move || watch_loop(tx, &[(watched, events)]));

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let quiet = tokio::time::timeout(QUIET_WINDOW, rx.changed()).await;
                assert!(quiet.is_err(), "the watchdog must not fire while the peer is still alive");

                close_peer();

                match tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => panic!("the channel closed without the watchdog ever sending `true` (fail-open path, not a fire)"),
                    Err(_) => panic!("the watchdog did not fire within the timeout after the peer closed"),
                }
            });
        })
        .join()
        .unwrap();
        handle.join().unwrap();
    }

    /// Regression test for the module's core claim: closing the *far* end
    /// of a pipe this process holds open must make the watchdog fire, even
    /// though nothing is reading or writing at the time — but not before.
    /// This is the fd-0 direction (a pipe read end, requesting `POLLIN` —
    /// see the sibling test below for the fd-1/`POLLOUT` direction).
    #[test]
    fn watch_loop_fires_on_pollhup_when_the_writer_closes_a_read_ends_peer() {
        let (read_fd, write_fd) = pipe_pair();
        // Closing the write end is what an externally-killed `ssh(1)`
        // effectively does to this process's stdin (fd 0, a pipe read end).
        assert_watch_loop_stays_quiet_then_fires(read_fd, libc::POLLIN, move || unsafe {
            libc::close(write_fd);
        });
        unsafe {
            libc::close(read_fd);
        }
    }

    /// The direction that actually matters in production: fd 1 (this
    /// process's stdout) is a pipe *write* end, requesting `POLLOUT` (never
    /// `POLLIN` — attaching a read filter to a write-only pipe end is
    /// exactly the kind of request Darwin's `poll()`-via-`kqueue` shim was
    /// suspected of turning into an immediate, unconditional `POLLNVAL`
    /// fire — see `assert_watch_loop_stays_quiet_then_fires`'s own docs for
    /// why the "stays quiet first" half of this test exists specifically to
    /// catch that).
    #[test]
    fn watch_loop_fires_on_pollerr_when_the_reader_closes_a_write_ends_peer() {
        let (read_fd, write_fd) = pipe_pair();
        // Closing the read end is what an externally-killed `ssh(1)`
        // effectively does to this process's stdout (fd 1, a pipe write end).
        assert_watch_loop_stays_quiet_then_fires(write_fd, libc::POLLOUT, move || unsafe {
            libc::close(read_fd);
        });
        unsafe {
            libc::close(write_fd);
        }
    }

    /// Regression test for the spurious-wakeup fix itself (the module doc's
    /// "why a sleep-guard" section): `watch_loop` must not spin when its fd
    /// becomes ready in its requested direction without ever closing. Sends
    /// one byte into a pipe it's watching for `POLLIN` and confirms the
    /// watchdog stays quiet well past one `SPURIOUS_BACKOFF` interval —
    /// proving the loop actually slept and re-polled rather than either
    /// firing on plain data (which would be wrong) or spinning (which an
    /// external test can't directly measure as CPU usage, but can catch
    /// indirectly here: a genuinely spinning loop still wouldn't fire
    /// early, so this doesn't fully replace manually confirming CPU usage,
    /// but does pin the "never treats readability alone as terminal" half).
    #[test]
    fn watch_loop_does_not_treat_plain_readability_as_terminal() {
        let (read_fd, write_fd) = pipe_pair();
        unsafe {
            assert_eq!(libc::write(write_fd, b"x".as_ptr().cast(), 1), 1);
        }
        assert_watch_loop_stays_quiet_then_fires(read_fd, libc::POLLIN, move || unsafe {
            libc::close(write_fd);
        });
        unsafe {
            libc::close(read_fd);
        }
    }

    /// Exercises the exact multi-fd shape `spawn()` actually passes to
    /// production (`&[(0, POLLIN), (1, POLLOUT)]`), not just each direction
    /// in isolation — the three tests above each watch a single fd, so none
    /// of them can catch a bug that only shows up when scanning *multiple*
    /// `pollfd`s together, e.g. an accidental `all()` instead of `any()` in
    /// the `TERMINAL` check, or only ever looking at `pollfds[0]`. That
    /// matters concretely here: since one of the two production fds
    /// (`POLLOUT` on fd 1) is essentially always spuriously ready, *every*
    /// production `poll()` call returns `ret > 0`, so correctness rests
    /// entirely on the scan correctly picking a terminal bit off the *other*
    /// fd out of a `pollfds` slice where not every entry is terminal (found
    /// necessary by adversarial review, 2026-09-02 — both reviewers
    /// independently flagged this as the one gap the single-fd tests can't
    /// close).
    ///
    /// Two independent pipes stand in for fd 0/fd 1, watched in the exact
    /// same `[(POLLIN fd, ...), (POLLOUT fd, ...)]` shape and order
    /// `spawn()` uses. `close_which` selects which of the two peers closes
    /// (and must make the watchdog fire) while the *other* fd stays
    /// permanently spuriously-ready the whole time — run as two separate
    /// tests below (closing each fd's peer in turn) so neither position is
    /// the only one ever proven capable of firing (adversarial review,
    /// 2026-09-02: closing only the `POLLIN` fd's peer, as an earlier draft
    /// of this test did, leaves the `POLLOUT` fd's own ability to fire
    /// completely unproven).
    ///
    /// `read_a` deliberately has an unread byte sitting in it for the whole
    /// run (see below) — this is not incidental to the `PollinSide` variant:
    /// production's real fd 0 always has `ssh(1)`'s version banner sitting
    /// unread the same way until the pump starts, so if a platform's
    /// `poll()` ever failed to report `POLLHUP` on a read end *while data is
    /// still buffered* (unverified — this is exactly the kind of Darwin
    /// `EVFILT_READ` behavior this module can't check without CI), this is
    /// the variant that would catch it, and only fd 1's `POLLOUT` (unrelated
    /// to buffered data at all) would still be covering production on that
    /// platform. If `PollinSide` alone fails on a given CI runner where
    /// `PolloutSide` passes, that's the diagnosis — and the fix is keeping
    /// both fds (already true today) plus a doc note, not new code.
    fn assert_watch_loop_fires_regardless_of_which_of_two_fds_closes(close_which: WhichFd) {
        let (read_a, write_a) = pipe_pair();
        let (read_b, write_b) = pipe_pair();
        // Make both fds spuriously-ready for the whole quiet window: `read_a`
        // gets a byte to sit unread (`POLLIN`-ready); `write_b`'s peer
        // (`read_b`) stays open, so `write_b` stays `POLLOUT`-ready.
        unsafe {
            assert_eq!(libc::write(write_a, b"x".as_ptr().cast(), 1), 1);
        }

        let (tx, mut rx) = watch::channel(false);
        let handle = std::thread::spawn(move || watch_loop(tx, &[(read_a, libc::POLLIN), (write_b, libc::POLLOUT)]));

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let quiet = tokio::time::timeout(QUIET_WINDOW, rx.changed()).await;
                assert!(quiet.is_err(), "the watchdog must not fire while both peers are still alive, even with both fds continuously spuriously-ready");

                match close_which {
                    WhichFd::PollinSide => unsafe {
                        libc::close(write_a);
                    },
                    WhichFd::PolloutSide => unsafe {
                        libc::close(read_b);
                    },
                }

                match tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => panic!("the channel closed without the watchdog ever sending `true` (fail-open path, not a fire)"),
                    Err(_) => panic!("the watchdog did not fire within the timeout after the peer closed"),
                }
            });
        })
        .join()
        .unwrap();
        handle.join().unwrap();

        unsafe {
            libc::close(read_a);
            libc::close(write_b);
            if !matches!(close_which, WhichFd::PollinSide) {
                libc::close(write_a);
            }
            if !matches!(close_which, WhichFd::PolloutSide) {
                libc::close(read_b);
            }
        }
    }

    #[derive(Clone, Copy)]
    enum WhichFd {
        PollinSide,
        PolloutSide,
    }

    #[test]
    fn watch_loop_fires_on_the_pollin_side_while_the_pollout_side_stays_spuriously_ready() {
        assert_watch_loop_fires_regardless_of_which_of_two_fds_closes(WhichFd::PollinSide);
    }

    #[test]
    fn watch_loop_fires_on_the_pollout_side_while_the_pollin_side_stays_spuriously_ready() {
        assert_watch_loop_fires_regardless_of_which_of_two_fds_closes(WhichFd::PolloutSide);
    }
}
