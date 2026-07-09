//! Signed release-artifact verification (`ISEKAI_PIPE_DESIGN.md` §8 Epic D).
//!
//! Bootstrap deployment (`isekai-bootstrap::openssh::OpenSshBackend`) today
//! only compares a locally computed SHA-256 against what a human reads off
//! the terminal at `init`/wrapper-bootstrap confirmation time
//! (`isekai_trust::schema::HelperTrust::trusted_helper_sha256`,
//! `UpdatePolicy::ExactDigestOnly`) — sufficient against accidental
//! corruption, but not against a compromised distribution point serving a
//! tampered binary alongside a matching-but-also-tampered digest. This
//! crate adds a second, independent check: a [`ReleaseManifest`] naming the
//! artifact's version/platform/architecture/size/digest, signed by an
//! offline ed25519 release-signing key whose public half is provisioned
//! into a [`TrustedReleaseKeys`] registry out of band (embedded constant,
//! shipped file, or CLI flag — this crate takes no position on which; see
//! `ISEKAI_PIPE_DESIGN.md` §8 Epic D for the still-open key-provisioning
//! and rotation policy).
//!
//! This crate is I/O-free except for the (infallible, pure) JSON
//! (de)serialization of [`SignedManifest`] — no file reads, no
//! `isekai-bootstrap` dependency. A caller loads the manifest JSON and
//! artifact bytes itself and calls [`verify_artifact`].

mod keys;
mod manifest;
mod verify;

pub use keys::{KeyLoadError, TrustedReleaseKeys};
pub use manifest::{canonical_manifest_bytes, sign_manifest, ReleaseManifest, SignedManifest};
pub use verify::{verify_artifact, ExpectedTarget, VerifyError};
