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
//! 2. A client that has just been preempted (its sender was dropped by
//!    [`AttachSlot::install`]) but hasn't yet observed
//!    [`protocol::Frame::Preempted`](super::protocol::Frame::Preempted) and
//!    stopped could otherwise keep forwarding `Stdin`/`Resize` into what is
//!    now a *different* client's session. Every stdin-forwarding path must
//!    call [`AttachSlot::is_current`] under the same lock immediately
//!    before writing to the pty — see that method's doc comment.

use std::sync::Mutex;

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
}

pub(crate) struct AttachSlot {
    state: Mutex<SlotState>,
}

/// Bound on the per-occupant output channel. Generous for ordinary terminal
/// output chunk sizes; once full, [`AttachSlot::broadcast`] drops rather
/// than blocks (see the module docs) — the ring buffer is what a
/// reattaching client actually recovers from, not channel backpressure.
const OCCUPANT_CHANNEL_CAPACITY: usize = 256;

impl AttachSlot {
    pub(crate) fn new(ring_capacity: usize) -> Self {
        Self { state: Mutex::new(SlotState { generation: 0, occupant: None, ring: RingBuffer::new(ring_capacity) }) }
    }

    /// Called by the pty read loop with each chunk of output — appends it
    /// to the replay ring buffer and, if a client is currently attached,
    /// forwards it over that client's channel. Never blocks: `try_send`
    /// only, and a full channel (a slow-reading client) just drops this
    /// chunk for *that live delivery* — the ring buffer still has it for
    /// the next `install`'s replay. This method must be callable — and
    /// **is** called — even when `occupant` is `None`, since the pty itself
    /// must keep being drained regardless of whether anyone is watching (a
    /// pty whose master end nobody reads from fills its kernel buffer and
    /// blocks the shell writing to it, the classic dtach freeze bug this
    /// design avoids by never gating the read loop on attachment).
    pub(crate) fn broadcast(&self, data: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ring.append(data);
        if let Some(tx) = &state.occupant {
            let _ = tx.try_send(RelayMsg::Data(data.to_vec()));
        }
    }

    /// Tells the current occupant (if any) that the pty's child process
    /// exited with `code` — see [`RelayMsg::Exit`]'s doc comment for why
    /// this bypasses the ring buffer entirely rather than going through
    /// [`Self::broadcast`]. A no-op if nobody is currently attached; the
    /// exit is simply not observed live (the daemon is about to shut down
    /// regardless, per its "lifetime = shell lifetime" design — there is no
    /// later reattach to replay this to).
    pub(crate) fn notify_exit(&self, code: u8) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = &state.occupant {
            let _ = tx.try_send(RelayMsg::Exit(code));
        }
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

        slot.vacate(gen2);
        assert!(!slot.is_current(gen2));
    }
}
