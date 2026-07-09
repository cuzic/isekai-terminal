//! I/O-less bootstrap planning layer shared across route types (direct/
//! STUN/relay) and topologies (0-hop/1-hop/multi-hop), per
//! `ISEKAI_PIPE_DESIGN.md` §8 Epic A. Implementing route selection and hop
//! traversal as separate ad-hoc logic per feature (wrapper auto-bootstrap,
//! `init`, future STUN/relay/multi-hop support) was rejected because the two
//! axes multiply combinatorially; this crate is the common layer they all
//! build on instead.
//!
//! This crate only defines *what a bootstrap attempt should do* and *how to
//! classify why it failed* — no `tokio`, no subprocess spawning, no network
//! I/O. Executing a [`BootstrapPlan`] (running `ssh(1)`, dialing QUIC,
//! persisting results) is the job of `isekai-bootstrap`/`isekai-ssh`/future
//! per-route executors (Epic G/H/I/K), which depend on this crate rather
//! than the other way around.

pub mod budget;
pub mod failure;
pub mod plan;

pub use budget::{BootstrapBudget, BootstrapPhase, BudgetError};
pub use failure::{classify_bootstrap_error, BootstrapFailure};
pub use plan::{BootstrapPlan, BootstrapTarget, CredentialSource, JumpHost, PersistencePolicy, PlanError, RouteKind, RoutePolicy};
