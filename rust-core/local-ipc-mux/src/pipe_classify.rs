//! Platform-generic classification of the raw OS errors a Windows named-pipe
//! claim/connect can produce, plus the client-side retry policy.
//!
//! The actual pipe syscalls only exist on `cfg(windows)`
//! ([`crate::WindowsNamedPipeChannel`]), and can't be exercised in this
//! project's Linux development environment. The *decisions* driven by those
//! syscalls' errors — "is this failure `AlreadyClaimed` or a real I/O error?",
//! "is this connect error `NotFound`, a transient race worth retrying, or
//! fatal?", "have we retried enough?" — are pure functions here so they can be
//! unit-tested on any host with fabricated inputs, instead of only ever being
//! reached on a real Windows box. (Same technique as
//! `isekai-ssh`'s `native::agent_auth::resolve_agent_target_from`.)

use std::io;
use std::time::Duration;

/// Win32 error code: the named pipe doesn't exist at all (`ClientOptions::open`
/// on a name no owner has created). Defined here rather than pulled from the
/// `windows` crate so this module compiles and its tests run on non-Windows
/// hosts.
pub(crate) const ERROR_FILE_NOT_FOUND: i32 = 2;

/// Win32 error code: the pipe exists but every instance is busy right now — no
/// free instance is waiting to accept. A real, transient race: the owner may
/// be between `accept()` calls, having connected its previous instance and not
/// yet created the next one.
pub(crate) const ERROR_PIPE_BUSY: i32 = 231;

/// How many times `connect` retries after an `ERROR_PIPE_BUSY` before giving
/// up. Five attempts at [`CONNECT_RETRY_BACKOFF`] apart covers the sub-second
/// window in which an owner is momentarily between accepting instances,
/// without hanging a client for long when there is genuinely no owner able to
/// serve it.
pub(crate) const CONNECT_MAX_RETRIES: usize = 5;

/// Delay between `connect` retries after an `ERROR_PIPE_BUSY`. Named-pipe
/// docs suggest `WaitNamedPipe`; a fixed short sleep is simpler and adequate
/// here because the contended window is tiny (one `ServerOptions::create`
/// call on the owner side).
pub(crate) const CONNECT_RETRY_BACKOFF: Duration = Duration::from_millis(75);

/// Whether a `try_claim` failure means "another process already owns this
/// name" rather than a genuine I/O failure.
///
/// `CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE` fails with
/// `ERROR_ACCESS_DENIED` — surfaced by the standard library as
/// [`io::ErrorKind::PermissionDenied`] — precisely when an instance of the
/// pipe already exists (i.e. another process claimed it first). Any other
/// error kind is a real failure to be reported as [`crate::ClaimError::Io`].
pub(crate) fn is_already_claimed(kind: io::ErrorKind) -> bool {
    kind == io::ErrorKind::PermissionDenied
}

/// What to do with a client-side `connect` attempt's OS error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectDisposition {
    /// No owner exists for this name — report [`crate::ConnectError::NotFound`]
    /// immediately, no retry.
    NotFound,
    /// The pipe exists but no free instance is ready this instant — a
    /// transient race worth a bounded retry.
    Retry,
    /// Any other error — give up with [`crate::ConnectError::Io`].
    Fatal,
}

/// Classifies a client-side `connect` OS error into a [`ConnectDisposition`].
/// Prefers the raw Win32 code (exact) and falls back to the portable
/// [`io::ErrorKind`] so a `NotFound` surfaced without a raw code is still
/// classified correctly.
pub(crate) fn classify_connect_error(
    raw_os_error: Option<i32>,
    kind: io::ErrorKind,
) -> ConnectDisposition {
    match raw_os_error {
        Some(ERROR_FILE_NOT_FOUND) => ConnectDisposition::NotFound,
        Some(ERROR_PIPE_BUSY) => ConnectDisposition::Retry,
        _ if kind == io::ErrorKind::NotFound => ConnectDisposition::NotFound,
        _ => ConnectDisposition::Fatal,
    }
}

/// Whether a `connect` attempt that produced `disposition` should be retried,
/// given how many retries have already happened. Only `Retry` dispositions
/// retry, and only up to `max_retries` times.
pub(crate) fn should_retry_connect(
    disposition: ConnectDisposition,
    retries_done: usize,
    max_retries: usize,
) -> bool {
    matches!(disposition, ConnectDisposition::Retry) && retries_done < max_retries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denied_is_the_only_already_claimed_signal() {
        assert!(is_already_claimed(io::ErrorKind::PermissionDenied));
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::Other,
            io::ErrorKind::ConnectionRefused,
        ] {
            assert!(!is_already_claimed(kind), "{kind:?} should not read as already-claimed");
        }
    }

    #[test]
    fn file_not_found_classifies_as_not_found() {
        assert_eq!(
            classify_connect_error(Some(ERROR_FILE_NOT_FOUND), io::ErrorKind::Other),
            ConnectDisposition::NotFound,
        );
    }

    #[test]
    fn pipe_busy_classifies_as_retry() {
        assert_eq!(
            classify_connect_error(Some(ERROR_PIPE_BUSY), io::ErrorKind::Other),
            ConnectDisposition::Retry,
        );
    }

    #[test]
    fn not_found_error_kind_without_a_raw_code_still_classifies_as_not_found() {
        assert_eq!(
            classify_connect_error(None, io::ErrorKind::NotFound),
            ConnectDisposition::NotFound,
        );
    }

    #[test]
    fn any_other_error_is_fatal() {
        assert_eq!(
            classify_connect_error(Some(5), io::ErrorKind::PermissionDenied),
            ConnectDisposition::Fatal,
        );
        assert_eq!(
            classify_connect_error(None, io::ErrorKind::ConnectionRefused),
            ConnectDisposition::Fatal,
        );
    }

    #[test]
    fn only_retry_dispositions_retry_and_only_up_to_the_cap() {
        assert!(should_retry_connect(ConnectDisposition::Retry, 0, CONNECT_MAX_RETRIES));
        assert!(should_retry_connect(ConnectDisposition::Retry, CONNECT_MAX_RETRIES - 1, CONNECT_MAX_RETRIES));
        // Exhausted the budget.
        assert!(!should_retry_connect(ConnectDisposition::Retry, CONNECT_MAX_RETRIES, CONNECT_MAX_RETRIES));
        // Non-retryable dispositions never retry, even with budget left.
        assert!(!should_retry_connect(ConnectDisposition::NotFound, 0, CONNECT_MAX_RETRIES));
        assert!(!should_retry_connect(ConnectDisposition::Fatal, 0, CONNECT_MAX_RETRIES));
    }
}
