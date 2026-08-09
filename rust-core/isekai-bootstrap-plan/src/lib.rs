//! I/O-less bootstrap planning layer shared across topologies (0-hop/1-hop/
//! multi-hop), per `ISEKAI_PIPE_DESIGN.md` §8 Epic A. Implementing hop-chain
//! validation and failure classification as separate ad-hoc logic per
//! feature (wrapper auto-bootstrap, `init`) was rejected because both need
//! the same checks; this crate is the common layer they both build on
//! instead.
//!
//! This crate only defines *how to validate a `--via` jump chain* and *how
//! to classify why a bootstrap attempt failed* — no `tokio`, no subprocess
//! spawning, no network I/O. Actually bootstrapping (running `ssh(1)`,
//! dialing QUIC, persisting results) is the job of
//! `isekai-bootstrap`/`isekai-ssh`.

pub mod failure;
pub mod plan;

pub use failure::{classify_bootstrap_error, BootstrapFailure};
pub use plan::{validate_jump_chain, BootstrapTarget, JumpHost, PlanError, MAX_JUMP_HOPS};
