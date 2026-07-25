//! `isekai-pipe claude-hookd` (`ISEKAI_PIPE_DESIGN.md` §8 Epic Q): a small
//! per-tab daemon that turns Claude Code hook events into a persistent,
//! debounced tab-color indicator via the existing `isekai-pipe ctl`
//! send path (`CtlMessage::SetTabColor`/`Notify`) — no new SSH/QUIC channel,
//! no new wire protocol, this is purely local plumbing on the remote host.
//!
//! Split into [`state`] (the actual decision logic: an I/O-free, unit-tested
//! pure function, `.claude/rules/rust-ssot.md`'s "state and decision logic
//! belong in one place" principle applied at the scale of this one small
//! feature) and everything else (CLI parsing, the async daemon loop, process
//! daemonization) as a thin wrapper around it.

pub(crate) mod state;
