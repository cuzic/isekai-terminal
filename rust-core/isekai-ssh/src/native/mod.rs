//! Windows-only native SSH client path for `isekai-ssh` (no `ssh(1)`
//! dependency) — `ISEKAI_PIPE_DESIGN.md`-adjacent design notes live in
//! `/home/cuzic/.claude/plans/fancy-humming-pnueli.md` M1. This module is
//! built and unit-tested on every platform (Linux CI is cheaper to run than
//! Windows), but only ever *invoked* on `cfg(windows)` — the Unix path
//! keeps shelling out to real `ssh(1)` via [`super::wrapper`] unchanged.

pub(crate) mod agent_auth;
pub(crate) mod bootstrap_backend;
pub(crate) mod child_stdio;
pub(crate) mod connect;
pub(crate) mod console;
pub(crate) mod host_key_trust;
pub(crate) mod private_key;
