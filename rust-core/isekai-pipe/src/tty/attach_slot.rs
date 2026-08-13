//! The daemon's single-occupant "who is currently attached" slot, plus the
//! replay ring buffer it shares one lock with (see [`AttachSlot::install`]'s
//! doc comment for why they must be updated together, not as two separate
//! locks).
//!
//! Generation-gated rather than a bare `Mutex<Option<Sender>>`: a naive
//! "just swap the sender" design has two failure modes a design review
//! caught before this shipped —
//!
//! 1. If the pty→client relay loop wrote to the occupant's channel *while
//!    holding the slot lock*, a hung/slow old client would block
//!    preemption entirely (the very thing preemption exists to route
//!    around). [`AttachSlot::broadcast`] instead only ever `try_send`s
//!    (never blocks) while the lock is held, and drops output for a
//!    slow/absent occupant on the floor — the ring buffer, not the live
//!    channel, is this feature's source of truth for "what did I miss."
//!    A dropped `try_send` is not silently accepted as permanent data
//!    loss, though: it flags [`SlotState::missed`], which
//!    [`AttachSlot::take_missed`] (called by `handle_client`'s writer task
//!    before forwarding *any* subsequent message, including `Exit`) turns
//!    into a full [`RingBuffer::replay`] resync — since `broadcast` always
//!    appends to the ring *before* attempting live delivery, the ring
//!    already has everything a drop could have lost, this just needs to
//!    reach the client before anything else does. Real gap found via
//!    pre-mortem review (2026-08-12): a bursty command's output right
//!    before it exits (a build's final lines, say) could previously
//!    outrun a slow/loaded client's channel and vanish with no recovery
//!    path at all — unlike a *reconnecting* client, which always got a
//!    correct ring-buffer replay via `install`, a client that stayed
//!    continuously attached through the drop had no equivalent.
//! 2. A client that has just been preempted (its sender was dropped by
//!    [`AttachSlot::install`]) but hasn't yet observed
//!    [`protocol::Frame::Preempted`](super::protocol::Frame::Preempted) and
//!    stopped could otherwise keep forwarding `Stdin`/`Resize` into what is
//!    now a *different* client's session. Every stdin-forwarding path must
//!    call [`AttachSlot::is_current`] under the same lock immediately
//!    before writing to the pty — see that method's doc comment.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

use super::ring_buffer::RingBuffer;

/// What the pty side has to tell the current occupant's writer task.
/// `Exit` is distinct from routing an `Exit` byte through `Data` (which
/// would need the writer task to parse pty output looking for a sentinel —
/// fragile and wrong) — the daemon's own child-exit detection
/// (`daemon.rs::run`) calls [`AttachSlot::notify_exit`] directly instead of
/// going through [`AttachSlot::broadcast`]'s ring-buffer-recording path,
/// since an exit code isn't something a *reattaching* client needs replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayMsg {
    Data(Vec<u8>),
    Exit(u8),
}

struct SlotState {
    /// Bumped by every [`AttachSlot::install`] call. `0` means "no client
    /// has ever attached yet" — real generations start at `1`, so a caller
    /// can never accidentally hold a generation that matches the initial
    /// state.
    generation: u64,
    /// `None` when nobody is currently attached (the shell keeps running
    /// and the ring buffer keeps filling regardless — see [`super::daemon`]'s
    /// "daemon lifetime = shell lifetime" design, not "= client attached").
    occupant: Option<mpsc::Sender<RelayMsg>>,
    ring: RingBuffer,
    /// Set by [`AttachSlot::broadcast`] when a live delivery `try_send` to
    /// the current occupant fails (its channel was full); consumed by
    /// [`AttachSlot::take_missed`]. Reset on every [`AttachSlot::install`]
    /// — a brand-new occupant already gets a correct full replay as part
    /// of attaching, so there is nothing for it to have "missed" yet.
    missed: bool,
}

pub(crate) struct AttachSlot {
    state: Mutex<SlotState>,
}

/// Bound on the per-occupant output channel. Generous for ordinary terminal
/// output chunk sizes; once full, [`AttachSlot::broadcast`] drops rather
/// than blocks (see the module docs) — a dropped live chunk is recovered
/// via [`AttachSlot::take_missed`]'s resync, not lost.
const OCCUPANT_CHANNEL_CAPACITY: usize = 256;

/// How many times [`AttachSlot::notify_exit`] retries `try_send` if the
/// occupant's channel is still full, and how long it waits between
/// attempts (~200ms total). Exists because `notify_exit` is only ever
/// called after `daemon.rs::run` has awaited `read_loop`'s own completion —
/// no more `broadcast()` calls can happen for this generation, so the
/// occupant channel's occupancy is monotonically shrinking as the writer
/// task drains it. A short bounded retry reliably catches "still full at
/// the exact instant we checked," which a single unretried `try_send`
/// cannot — the same class of bug [`AttachSlot::broadcast`]'s `missed`
/// flag fixes for ordinary data, applied here to the exit notification
/// itself (found while writing that fix: `notify_exit` had the identical
/// unretried-`try_send`-under-load gap).
const NOTIFY_EXIT_RETRY_DELAY: Duration = Duration::from_millis(20);
const NOTIFY_EXIT_RETRY_ATTEMPTS: u32 = 10;

impl AttachSlot {
    pub(crate) fn new(ring_capacity: usize) -> Self {
        Self { state: Mutex::new(SlotState { generation: 0, occupant: None, ring: RingBuffer::new(ring_capacity), missed: false }) }
    }

    /// Called by the pty read loop with each chunk of output — appends it
    /// to the replay ring buffer and, if a client is currently attached,
    /// forwards it over that client's channel. Never blocks: `try_send`
    /// only; a full channel (a slow-reading or momentarily-overwhelmed
    /// client) just drops this chunk for *that live delivery* and flags
    /// [`SlotState::missed`] instead — [`Self::take_missed`] turns that
    /// into a full resync from the ring buffer, which already has this
    /// chunk (appended just above, before the `try_send` attempt). This
    /// method must be callable — and **is** called — even when `occupant`
    /// is `None`, since the pty itself must keep being drained regardless
    /// of whether anyone is watching (a pty whose master end nobody reads
    /// from fills its kernel buffer and blocks the shell writing to it,
    /// the classic dtach freeze bug this design avoids by never gating the
    /// read loop on attachment).
    pub(crate) fn broadcast(&self, data: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ring.append(data);
        if let Some(tx) = &state.occupant {
            if tx.try_send(RelayMsg::Data(data.to_vec())).is_err() {
                state.missed = true;
            }
        }
    }

    /// Tells the current occupant (if any) that the pty's child process
    /// exited with `code` — see [`RelayMsg::Exit`]'s doc comment for why
    /// this bypasses the ring buffer entirely rather than going through
    /// [`Self::broadcast`]. A no-op if nobody is currently attached; the
    /// exit is simply not observed live (the daemon is about to shut down
    /// regardless, per its "lifetime = shell lifetime" design — there is no
    /// later reattach to replay this to). See [`NOTIFY_EXIT_RETRY_DELAY`]
    /// for why this retries rather than trying once.
    pub(crate) async fn notify_exit(&self, code: u8) {
        for attempt in 0..NOTIFY_EXIT_RETRY_ATTEMPTS {
            let sent = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                match &state.occupant {
                    Some(tx) => tx.try_send(RelayMsg::Exit(code)).is_ok(),
                    None => return, // nobody attached; nothing to notify
                }
            };
            if sent {
                return;
            }
            if attempt + 1 < NOTIFY_EXIT_RETRY_ATTEMPTS {
                tokio::time::sleep(NOTIFY_EXIT_RETRY_DELAY).await;
            }
        }
        log::warn!("isekai-pipe tty daemon: occupant channel still full after {NOTIFY_EXIT_RETRY_ATTEMPTS} retries; giving up on delivering Frame::Exit live (a reattach would still see the shell is gone)");
    }

    /// Returns `true` (and resets the flag) if [`Self::broadcast`] had to
    /// drop a live chunk for `generation` since the last call. `false`
    /// (without touching shared state) if `generation` is no longer
    /// current — a writer task about to be preempted has nothing useful to
    /// do with a flag that now belongs to whichever occupant replaced it.
    /// The caller (`daemon.rs::handle_client`'s writer task) is expected to
    /// call this before forwarding *every* message it dequeues — see
    /// [`Self::current_replay`] for what to send when this returns `true`.
    pub(crate) fn take_missed(&self, generation: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != generation {
            return false;
        }
        std::mem::take(&mut state.missed)
    }

    /// A fresh snapshot of the ring buffer — the same soft-reset-prefixed
    /// replay [`Self::install`] hands a brand-new attach — for the writer
    /// task to send after [`Self::take_missed`] returns `true`. Taken
    /// *after* `take_missed` observed the drop, so it necessarily includes
    /// everything the drop lost (and possibly more, appended since) —
    /// whatever individual chunk was being processed when `take_missed`
    /// fired is redundant with this and should be discarded, not also
    /// forwarded (see the call site).
    pub(crate) fn current_replay(&self) -> Vec<u8> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).ring.replay()
    }

    /// Installs a fresh occupant, preempting whatever was there before (its
    /// `Sender` is dropped here; the old occupant's own writer task, once
    /// it observes the paired `Receiver` returning `None`, is what actually
    /// sends it `Frame::Preempted` and exits — this method itself only
    /// performs the swap). Returns the new generation (the caller must
    /// track this and pass it to [`Self::is_current`]/[`Self::vacate`]) and
    /// a snapshot of the replay buffer for the caller to send the new
    /// client directly (not routed back through the channel this same call
    /// just created — that channel is for *future* pty output only).
    ///
    /// The swap and the ring snapshot happen under one lock acquisition
    /// deliberately: taking them as two separate locked sections would open
    /// a window where [`Self::broadcast`] could append output between the
    /// snapshot and the install that neither the snapshot (already taken)
    /// nor the new occupant's channel (not installed yet) would ever
    /// deliver — a real, silent gap in the replayed transcript.
    pub(crate) fn install(&self, tx: mpsc::Sender<RelayMsg>) -> (u64, Vec<u8>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation += 1;
        state.occupant = Some(tx);
        state.missed = false;
        (state.generation, state.ring.replay())
    }

    /// `true` if `generation` is still the current occupant's generation.
    /// Every path that forwards a client's own `Stdin`/`Resize` into the
    /// pty must call this — under no other lock in between — immediately
    /// before doing so. A client that was just preempted sees `false` here
    /// even if it hasn't yet processed the channel closure that will
    /// eventually make it stop on its own, closing the window where stale
    /// input could otherwise reach a different client's session.
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).generation == generation
    }

    /// Clears the occupant slot, but only if `generation` is still current
    /// — an ordinary (not preempted) disconnect calls this on its way out.
    /// A stale call from a client that was itself already preempted by a
    /// newer one must not clear the *newer* occupant out from under it,
    /// which the generation check prevents.
    pub(crate) fn vacate(&self, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation == generation {
            state.occupant = None;
        }
    }

    pub(crate) fn new_occupant_channel() -> (mpsc::Sender<RelayMsg>, mpsc::Receiver<RelayMsg>) {
        mpsc::channel(OCCUPANT_CHANNEL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_bumps_generation_starting_from_one() {
        let slot = AttachSlot::new(1024);
        let (tx, _rx) = AttachSlot::new_occupant_channel();
        let (gen1, _) = slot.install(tx);
        assert_eq!(gen1, 1);
    }

    /// Regression for a real bug found via pre-mortem review (2026-08-12):
    /// `RingBuffer::replay()` used to prepend the soft-reset sequence (`ESC
    /// c` / RIS) *unconditionally*, so the very first `install()` on a
    /// session nobody has ever broadcast to — the literal "first ever tty
    /// attach" case — handed the caller a non-empty replay whose only
    /// content was a full terminal reset, which `daemon.rs` then sent
    /// straight to the connecting client's real terminal, silently wiping
    /// its scrollback. This is checked here rather than by spawning a real
    /// shell (as an earlier version of this fix's regression test did)
    /// because a real shell's own startup output (e.g. macOS's default
    /// bash printing "The default interactive shell is now zsh...") can
    /// legitimately race into the ring buffer before a client's `install()`
    /// call — at which point a non-empty, soft-reset-prefixed replay is
    /// correct, not a bug. `AttachSlot::install` on a slot nobody has ever
    /// called [`AttachSlot::broadcast`] on is the one case where "empty"
    /// is guaranteed without racing real process I/O.
    #[test]
    fn first_ever_install_on_a_slot_nobody_has_broadcast_to_returns_no_replay_at_all() {
        let slot = AttachSlot::new(1024);
        let (tx, _rx) = AttachSlot::new_occupant_channel();
        let (_gen, replay) = slot.install(tx);
        assert_eq!(replay, Vec::<u8>::new(), "must not send a soft-reset when there is nothing buffered to protect");
    }

    #[test]
    fn a_second_install_preempts_the_first_and_bumps_generation_again() {
        let slot = AttachSlot::new(1024);
        let (tx1, mut rx1) = AttachSlot::new_occupant_channel();
        let (gen1, _) = slot.install(tx1);
        let (tx2, _rx2) = AttachSlot::new_occupant_channel();
        let (gen2, _) = slot.install(tx2);

        assert_eq!(gen2, gen1 + 1);
        assert!(!slot.is_current(gen1), "the first occupant's generation must no longer be current");
        assert!(slot.is_current(gen2));
        // The first occupant's sender was dropped -- its receiver observes
        // closure (this is what a real writer task would use to trigger
        // sending Frame::Preempted).
        assert_eq!(rx1.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected));
    }

    #[test]
    fn broadcast_reaches_the_current_occupant_and_the_ring_buffer() {
        let slot = AttachSlot::new(1024);
        let (tx, mut rx) = AttachSlot::new_occupant_channel();
        slot.install(tx);

        slot.broadcast(b"hello");

        assert_eq!(rx.try_recv().unwrap(), RelayMsg::Data(b"hello".to_vec()));
        let (_gen, replay) = {
            let (tx2, _rx2) = AttachSlot::new_occupant_channel();
            slot.install(tx2)
        };
        assert!(replay.ends_with(b"hello"), "the ring buffer must retain output even after live delivery: {replay:?}");
    }

    #[test]
    fn broadcast_with_no_occupant_still_fills_the_ring_buffer() {
        let slot = AttachSlot::new(1024);
        slot.broadcast(b"nobody watching");
        let (tx, _rx) = AttachSlot::new_occupant_channel();
        let (_gen, replay) = slot.install(tx);
        assert!(replay.ends_with(b"nobody watching"));
    }

    #[test]
    fn vacate_only_clears_the_matching_generation() {
        let slot = AttachSlot::new(1024);
        let (tx1, _rx1) = AttachSlot::new_occupant_channel();
        let (gen1, _) = slot.install(tx1);
        let (tx2, mut rx2) = AttachSlot::new_occupant_channel();
        let (gen2, _) = slot.install(tx2);

        // A stale vacate from the already-preempted first occupant must not
        // clear the second (current) occupant.
        slot.vacate(gen1);
        slot.broadcast(b"still here");
        assert_eq!(
            rx2.try_recv().unwrap(),
            RelayMsg::Data(b"still here".to_vec()),
            "vacate(stale generation) must not have cleared the current occupant"
        );

        // `vacate` only clears the occupant slot -- it deliberately does
        // *not* bump `generation` (that counter exists solely to detect a
        // *newer* `install`/preemption, not general occupancy), so
        // `is_current(gen2)` correctly stays `true` here; what must actually
        // change is that the occupant's sender is dropped (its receiver
        // observes disconnection, same as an ordinary preemption) and a
        // later broadcast has nothing to deliver to.
        slot.vacate(gen2);
        assert!(slot.is_current(gen2), "vacate must not itself invalidate the generation counter");
        assert_eq!(rx2.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected), "vacate must drop the occupant's sender");
        slot.broadcast(b"nobody here now"); // must not panic with no occupant present
    }

    /// Fills `tx`'s channel to `OCCUPANT_CHANNEL_CAPACITY` without a
    /// receiver draining it, so the *next* `broadcast()` is guaranteed to
    /// hit a full channel and set `missed`.
    fn fill_occupant_channel(tx: &mpsc::Sender<RelayMsg>) {
        for _ in 0..OCCUPANT_CHANNEL_CAPACITY {
            tx.try_send(RelayMsg::Data(Vec::new())).expect("channel must not already be full");
        }
    }

    /// Regression coverage for a real gap found via pre-mortem review
    /// (2026-08-12, `broadcast`'s own doc comment has the full mechanism):
    /// a live client that stayed continuously attached through a
    /// full-channel drop used to have no way to ever recover the dropped
    /// bytes — unlike a *reconnecting* client, which always got a correct
    /// `install` replay. `take_missed` + `current_replay` close this: the
    /// ring buffer already has everything (`broadcast` appends before
    /// attempting delivery), so a resync is always possible.
    #[test]
    fn broadcast_flags_missed_once_the_occupant_channel_is_full() {
        let slot = AttachSlot::new(1024);
        let (tx, _rx) = AttachSlot::new_occupant_channel();
        let (generation, _) = slot.install(tx.clone());
        fill_occupant_channel(&tx);

        assert!(!slot.take_missed(generation), "must not report a drop before any has actually happened");

        slot.broadcast(b"one chunk too many");
        assert!(slot.take_missed(generation), "a broadcast that overflows the full occupant channel must flag missed");
    }

    #[test]
    fn take_missed_is_consumed_by_a_single_call() {
        let slot = AttachSlot::new(1024);
        let (tx, _rx) = AttachSlot::new_occupant_channel();
        let (generation, _) = slot.install(tx.clone());
        fill_occupant_channel(&tx);
        slot.broadcast(b"dropped");

        assert!(slot.take_missed(generation), "first call must observe the flag");
        assert!(!slot.take_missed(generation), "a second call must not still report the same drop");
    }

    #[test]
    fn take_missed_returns_false_and_does_not_clear_a_stale_generations_flag() {
        let slot = AttachSlot::new(1024);
        let (tx1, _rx1) = AttachSlot::new_occupant_channel();
        let (gen1, _) = slot.install(tx1);

        // A second occupant preempts the first — `install` resets `missed`
        // for the fresh occupant, then this occupant's own drop sets it
        // again for `gen2` specifically.
        let (tx2, _rx2) = AttachSlot::new_occupant_channel();
        let (gen2, _) = slot.install(tx2.clone());
        fill_occupant_channel(&tx2);
        slot.broadcast(b"dropped for gen2");

        assert!(!slot.take_missed(gen1), "a stale (preempted) generation must never observe another occupant's flag");
        assert!(slot.take_missed(gen2), "the flag belongs to the current occupant and must still be observable after the stale check above");
    }

    #[test]
    fn install_resets_missed_for_the_fresh_occupant() {
        let slot = AttachSlot::new(1024);
        let (tx1, _rx1) = AttachSlot::new_occupant_channel();
        let (_gen1, _) = slot.install(tx1.clone());
        fill_occupant_channel(&tx1);
        slot.broadcast(b"dropped before the reattach");

        let (tx2, _rx2) = AttachSlot::new_occupant_channel();
        let (gen2, _) = slot.install(tx2);
        assert!(
            !slot.take_missed(gen2),
            "a brand-new occupant already gets a correct full replay from install() itself — it must not immediately \
             see a stale missed flag left over from the occupant it just preempted"
        );
    }

    #[test]
    fn current_replay_includes_everything_broadcast_so_far_including_what_was_just_dropped() {
        let slot = AttachSlot::new(1024);
        let (tx, _rx) = AttachSlot::new_occupant_channel();
        slot.install(tx.clone());
        fill_occupant_channel(&tx);

        slot.broadcast(b"lost to the full channel");
        slot.broadcast(b" but not to the ring");

        let replay = slot.current_replay();
        assert!(
            replay.ends_with(b"lost to the full channel but not to the ring"),
            "current_replay must include every byte ever broadcast, including chunks the live channel dropped: {replay:?}"
        );
    }

    /// `notify_exit`'s retry (not `broadcast`'s `missed` flag — `Exit`
    /// bypasses the ring buffer entirely, see `RelayMsg::Exit`'s doc
    /// comment) must succeed once the channel has room again, not give up
    /// after a single failed `try_send`.
    #[tokio::test]
    async fn notify_exit_retries_until_the_occupant_channel_has_room() {
        let slot = AttachSlot::new(1024);
        let (tx, mut rx) = AttachSlot::new_occupant_channel();
        let (generation, _) = slot.install(tx.clone());
        fill_occupant_channel(&tx);

        // Drain exactly one slot shortly after `notify_exit` starts
        // retrying — well within its ~200ms total retry budget — so the
        // first `try_send` attempt is guaranteed to fail (channel still
        // full) but a later retry succeeds. Hands `rx` back out so the
        // test can keep draining it afterward.
        let drain_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let freed = rx.recv().await;
            (freed, rx)
        });

        tokio::time::timeout(Duration::from_millis(500), slot.notify_exit(7))
            .await
            .expect("notify_exit must not hang waiting for room that does eventually appear");
        assert!(!slot.take_missed(generation), "Exit delivery must not itself be reported through the Data-only `missed` flag");

        let (freed_slot, mut rx) = drain_task.await.unwrap();
        assert_eq!(freed_slot, Some(RelayMsg::Data(Vec::new())), "sanity: the freed slot was the pre-filled dummy data");

        // `notify_exit`'s successful `try_send` filled the *one* slot
        // `drain_task` just freed — it does not jump the queue, so the
        // remaining pre-filled dummy messages (255 of them) are still
        // ahead of it. Drain everything left and confirm `Exit(7)` shows
        // up exactly once, as the very last message (FIFO: it was enqueued
        // after every dummy still in the channel at that point).
        let mut remaining = Vec::new();
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            remaining.push(msg);
        }
        assert_eq!(
            remaining.last(),
            Some(&RelayMsg::Exit(7)),
            "Exit(7) must eventually be delivered, as the last message once every earlier one has drained: {remaining:?}"
        );
        assert_eq!(
            remaining.iter().filter(|m| **m == RelayMsg::Exit(7)).count(),
            1,
            "Exit must be delivered exactly once, not retried again after it already succeeded: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn notify_exit_returns_immediately_when_nobody_is_attached() {
        let slot = AttachSlot::new(1024);
        // No install() call at all -- occupant is None.
        tokio::time::timeout(Duration::from_millis(50), slot.notify_exit(1))
            .await
            .expect("notify_exit must return immediately, not retry, when there is no occupant at all");
    }
}
