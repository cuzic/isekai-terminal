//! [`race_with_stagger`]: a Happy-Eyeballs-style (RFC 8305) two-future race
//! with a staggered start, extracted from `isekai-transport`'s original
//! `race_direct_and_relay` (`ISEKAI_PIPE_DESIGN.md` task `#19`) once it
//! became clear the racing mechanics themselves don't depend on QUIC, mux
//! backends, or any isekai-specific type — it's a pure async combinator
//! over two arbitrary `Future<Output = Result<T, E>>`s. Everything
//! isekai-specific about that original function (shared fencing identity,
//! `AttemptFailure` classification, `RaceWinner`/`RaceConnectError`) stays in
//! `isekai-transport::race`, which now calls this combinator instead of
//! hand-rolling the `tokio::select!` itself.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Which future actually produced the returned value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    A,
    B,
}

/// Races `fut_a` against `fut_b`: `fut_a` starts immediately, `fut_b` is only
/// polled for the first time once `stagger` has elapsed without `fut_a`
/// finishing (success *or* failure) — launching both at once tends to let
/// whichever candidate is topologically shorter win essentially every time,
/// defeating the point of preferring one when it's actually reachable
/// (RFC 8305 "Happy Eyeballs" existing for the identical reason on the
/// connect-vs-connect axis).
///
/// Because Rust futures are lazy (constructing one runs no code until it is
/// first polled), simply *not polling* `fut_b` until the stagger window
/// elapses is enough to implement "B doesn't start yet" — no explicit
/// start-delay bookkeeping is needed here.
///
/// Once both futures are in flight, whichever resolves first wins outright
/// on success; if the first to resolve is an `Err`, this waits on the other
/// one rather than failing immediately — both must fail for this function
/// to return `Err`. The loser (whichever future was still in flight when the
/// other one won) is simply dropped in place; this function has no opinion
/// on cancellation semantics beyond that.
pub async fn race_with_stagger<T, E>(
    fut_a: impl Future<Output = Result<T, E>>,
    fut_b: impl Future<Output = Result<T, E>>,
    stagger: Duration,
) -> Result<(Winner, T), (E, E)> {
    let mut fut_a: Pin<Box<dyn Future<Output = Result<T, E>>>> = Box::pin(fut_a);
    let mut fut_b: Pin<Box<dyn Future<Output = Result<T, E>>>> = Box::pin(fut_b);

    // Phase 1: give `fut_a` a head start. If it finishes (either way) before
    // `stagger` elapses, `fut_b` never even starts.
    if let Ok(result_a) = tokio::time::timeout(stagger, fut_a.as_mut()).await {
        return match result_a {
            Ok(value) => Ok((Winner::A, value)),
            Err(err_a) => match fut_b.await {
                Ok(value) => Ok((Winner::B, value)),
                Err(err_b) => Err((err_a, err_b)),
            },
        };
    }

    // Phase 2: `fut_a` is still running past the stagger window; `fut_b`
    // joins the race.
    tokio::select! {
        result_a = fut_a.as_mut() => {
            match result_a {
                Ok(value) => Ok((Winner::A, value)),
                Err(err_a) => match fut_b.await {
                    Ok(value) => Ok((Winner::B, value)),
                    Err(err_b) => Err((err_a, err_b)),
                },
            }
        }
        result_b = fut_b.as_mut() => {
            match result_b {
                Ok(value) => Ok((Winner::B, value)),
                Err(err_b) => match fut_a.await {
                    Ok(value) => Ok((Winner::A, value)),
                    Err(err_a) => Err((err_a, err_b)),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    async fn ok_after(millis: u64, value: &'static str) -> Result<&'static str, &'static str> {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        Ok(value)
    }

    async fn err_after(millis: u64, value: &'static str) -> Result<&'static str, &'static str> {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        Err(value)
    }

    #[tokio::test(start_paused = true)]
    async fn a_wins_within_stagger_window_without_starting_b() {
        let b_started = std::sync::Arc::new(AtomicBool::new(false));
        let b_started2 = b_started.clone();
        let fut_a = ok_after(10, "a");
        let fut_b = async move {
            b_started2.store(true, Ordering::SeqCst);
            ok_after(10, "b").await
        };

        let result = race_with_stagger(fut_a, fut_b, Duration::from_millis(250)).await;

        assert_eq!(result.unwrap(), (Winner::A, "a"));
        assert!(!b_started.load(Ordering::SeqCst), "b must not start when a wins within the stagger window");
    }

    #[tokio::test(start_paused = true)]
    async fn b_joins_and_wins_after_stagger_elapses() {
        let fut_a = ok_after(1000, "a");
        let fut_b = ok_after(10, "b");

        let result = race_with_stagger(fut_a, fut_b, Duration::from_millis(50)).await;

        assert_eq!(result.unwrap(), (Winner::B, "b"));
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_to_b_when_a_fails_within_stagger_window() {
        let fut_a = err_after(10, "a failed");
        let fut_b = ok_after(10, "b");

        let result = race_with_stagger(fut_a, fut_b, Duration::from_millis(250)).await;

        assert_eq!(result.unwrap(), (Winner::B, "b"));
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_to_a_when_b_wins_the_race_but_fails() {
        let fut_a = ok_after(200, "a");
        let fut_b = err_after(10, "b failed");

        let result = race_with_stagger(fut_a, fut_b, Duration::from_millis(50)).await;

        assert_eq!(result.unwrap(), (Winner::A, "a"));
    }

    #[tokio::test(start_paused = true)]
    async fn both_failing_returns_both_errors() {
        let fut_a = err_after(10, "a failed");
        let fut_b = err_after(10, "b failed");

        let result = race_with_stagger(fut_a, fut_b, Duration::from_millis(250)).await;

        assert_eq!(result.unwrap_err(), ("a failed", "b failed"));
    }
}
