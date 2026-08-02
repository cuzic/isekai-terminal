//! The pure decision core of `claude-hookd` (`ISEKAI_PIPE_DESIGN.md` §8
//! Epic Q): given the tab's current [`TabState`], a hook event for one
//! Claude Code session, and the current time, decides the next state and
//! which [`Action`]s (if any) the async driver should perform.
//!
//! Deliberately I/O-free and takes `now`/`timeout` as parameters rather than
//! reading real time internally — the same "pure `(state, event) -> (state,
//! actions)` function, tested without waiting on real timers" shape as
//! `isekai-bootstrap-plan`'s `BootstrapPlan`. Every transition, including
//! both timeout paths, is exercised in this module's tests in microseconds,
//! not the real 10-minute attention timeout.
//!
//! State is keyed per **Claude Code session** (`session_id` from the hook
//! JSON payload — one tmux pane = one session), not per tab
//! (`$ISEKAI_CTL_SOCK`, shared by every pane in the same SSH connection).
//! Keying per tab instead of per session was the original (buggy) design:
//! resolving one pane's session would silently clear another pane's still-
//! pending attention, and vice versa (see the multi-session tests below,
//! `multiple_sessions_in_a_tab` — this module's test suite exists
//! specifically to pin the fix for that bug).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One hook event, already reduced from Claude Code's hook JSON payload
/// (`hook_event_name`/`tool_name`) by the caller — this module knows nothing
/// about Claude Code's wire format, only "a session needs attention" or "a
/// session is resolved".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookEvent {
    /// `Notification`/`Stop`, or `PreToolUse` matched to `AskUserQuestion`.
    Notify,
    /// `UserPromptSubmit`, or `PostToolUse` matched to `AskUserQuestion`.
    Resolve,
}

/// An effect the async driver should perform in response to a transition.
/// Never inferred from the *current* state alone — only ever returned
/// alongside the transition that caused it, so the driver never has to
/// re-derive "did the aggregate actually change" itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// The tab's aggregate state just became "needs attention" (no session
    /// was in `Attention` before this transition, exactly one is now).
    /// Fired **once** per idle→attention transition, not on every debounce
    /// refresh or every additional session that joins an already-attention
    /// tab — see this module's docs on why the popup isn't per-event.
    SetAttentionColorAndPopup,
    /// The tab's aggregate state just became "idle" (the last `Attention`
    /// session just resolved or timed out).
    SetIdleColor,
}

/// A tab's state: the set of Claude Code sessions currently in `Attention`,
/// each with the deadline (`now + timeout` as of their most recent `Notify`)
/// at which they revert to `Idle` if untouched. A session absent from this
/// map is implicitly `Idle` — there is no need to track idle sessions, so
/// this map is empty exactly when the tab's aggregate color is idle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TabState {
    attention: HashMap<String, Instant>,
}

impl TabState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether the tab's aggregate color should currently be the attention
    /// color — true iff at least one session is in `Attention`.
    pub(crate) fn is_attention(&self) -> bool {
        !self.attention.is_empty()
    }

    /// The earliest pending deadline across all `Attention` sessions, or
    /// `None` if the tab is fully idle. The async driver sleeps until this
    /// instant (via `tokio::select!` racing new events, per the design doc)
    /// rather than polling.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.attention.values().copied().min()
    }

    #[cfg(test)]
    pub(crate) fn attention_session_count(&self) -> usize {
        self.attention.len()
    }
}

/// Applies one hook event for `session_id` and returns the new state plus
/// whatever `Action`s the aggregate transition requires.
pub(crate) fn apply_event(state: &TabState, session_id: &str, event: HookEvent, now: Instant, timeout: Duration) -> (TabState, Vec<Action>) {
    let was_attention = state.is_attention();
    let mut next = state.clone();
    match event {
        HookEvent::Notify => {
            // Also covers the debounce case (session already in `Attention`):
            // `insert` on an existing key just replaces the deadline, which
            // is exactly "refresh the timeout".
            next.attention.insert(session_id.to_string(), now + timeout);
        }
        HookEvent::Resolve => {
            // A `Resolve` for a session that was never `Attention` (or
            // already resolved/timed out) is a no-op removal — deliberately
            // not an error, since `PostToolUse(AskUserQuestion)` firing
            // without a preceding `PreToolUse` Notify (e.g. daemon restarted
            // mid-question) must not panic or otherwise misbehave.
            next.attention.remove(session_id);
        }
    }
    (next.clone(), actions_for_transition(was_attention, next.is_attention()))
}

/// Sweeps every session whose deadline has passed as of `now` to `Idle` and
/// returns the new state plus whatever `Action`s the resulting aggregate
/// transition requires. Called by the async driver when its
/// `next_deadline()`-derived sleep fires — a pure sibling of [`apply_event`]
/// for the "nothing happened, but time did" case.
pub(crate) fn apply_timeout(state: &TabState, now: Instant) -> (TabState, Vec<Action>) {
    let was_attention = state.is_attention();
    let mut next = state.clone();
    next.attention.retain(|_, deadline| *deadline > now);
    (next.clone(), actions_for_transition(was_attention, next.is_attention()))
}

fn actions_for_transition(was_attention: bool, is_attention: bool) -> Vec<Action> {
    match (was_attention, is_attention) {
        (false, true) => vec![Action::SetAttentionColorAndPopup],
        (true, false) => vec![Action::SetIdleColor],
        // (false, false): a `Resolve`/timeout that didn't actually change
        // the aggregate (already idle). (true, true): entry into an
        // already-attention tab, a debounce refresh, or one of several
        // sessions resolving while another is still `Attention` — none of
        // these are visible to the driver, by design (see `Action`'s docs).
        (false, false) | (true, true) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(600);

    #[test]
    fn first_notify_enters_attention_and_fires_popup() {
        let now = Instant::now();
        let (state, actions) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        assert!(state.is_attention());
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup]);
        assert_eq!(state.next_deadline(), Some(now + TIMEOUT));
    }

    #[test]
    fn second_notify_for_the_same_session_debounces_without_reissuing_actions() {
        let now = Instant::now();
        let (state, _) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        let later = now + Duration::from_secs(60);
        let (state, actions) = apply_event(&state, "s1", HookEvent::Notify, later, TIMEOUT);
        assert!(actions.is_empty(), "debounce refresh must not resend the popup/color");
        // The deadline moved forward — this is the actual debounce effect.
        assert_eq!(state.next_deadline(), Some(later + TIMEOUT));
    }

    #[test]
    fn resolve_of_the_only_attention_session_returns_to_idle() {
        let now = Instant::now();
        let (state, _) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        let (state, actions) = apply_event(&state, "s1", HookEvent::Resolve, now, TIMEOUT);
        assert!(!state.is_attention());
        assert_eq!(actions, vec![Action::SetIdleColor]);
    }

    #[test]
    fn resolve_of_a_session_that_was_never_attention_is_a_silent_no_op() {
        let now = Instant::now();
        let (state, actions) = apply_event(&TabState::new(), "ghost", HookEvent::Resolve, now, TIMEOUT);
        assert!(!state.is_attention());
        assert!(actions.is_empty());
    }

    #[test]
    fn timeout_before_deadline_does_nothing() {
        let now = Instant::now();
        let (state, _) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        let (state, actions) = apply_timeout(&state, now + Duration::from_secs(1));
        assert!(state.is_attention());
        assert!(actions.is_empty());
    }

    #[test]
    fn timeout_after_deadline_returns_to_idle() {
        let now = Instant::now();
        let (state, _) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        let (state, actions) = apply_timeout(&state, now + TIMEOUT + Duration::from_secs(1));
        assert!(!state.is_attention());
        assert_eq!(actions, vec![Action::SetIdleColor]);
    }

    /// Pins the fix for the bug the per-session (rather than per-tab) design
    /// exists to close: two tmux panes (two Claude Code sessions) sharing
    /// one tab must not resolve/suppress each other's attention state.
    #[test]
    fn multiple_sessions_in_a_tab_do_not_interfere() {
        let now = Instant::now();

        // Pane A asks a question — tab goes attention, popup fires once.
        let (state, actions) = apply_event(&TabState::new(), "pane-a", HookEvent::Notify, now, TIMEOUT);
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup]);

        // Pane B's Stop fires while A is still pending — no second popup,
        // the tab was already attention (this is the debounce-suppression
        // half of the bug: a naive "assert PreToolUse->PostToolUse pairing
        // only" design would have wrongly treated this as A's own debounce).
        let (state, actions) = apply_event(&state, "pane-b", HookEvent::Notify, now, TIMEOUT);
        assert!(actions.is_empty());
        assert_eq!(state.attention_session_count(), 2);

        // The user answers pane B first. Pane A is still blocked, so the
        // tab must stay in the attention color — this is the bug's other
        // half: a per-tab (not per-session) design would have wrongly
        // reverted to idle here, hiding A's still-pending question.
        let (state, actions) = apply_event(&state, "pane-b", HookEvent::Resolve, now, TIMEOUT);
        assert!(actions.is_empty(), "pane A is still attention; the tab must not revert to idle");
        assert!(state.is_attention());

        // Now pane A resolves too — only *now* does the tab revert.
        let (state, actions) = apply_event(&state, "pane-a", HookEvent::Resolve, now, TIMEOUT);
        assert_eq!(actions, vec![Action::SetIdleColor]);
        assert!(!state.is_attention());
    }

    #[test]
    fn next_deadline_is_the_earliest_across_sessions() {
        let now = Instant::now();
        let (state, _) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        let later = now + Duration::from_secs(30);
        let (state, _) = apply_event(&state, "s2", HookEvent::Notify, later, TIMEOUT);
        // s1's deadline (now + TIMEOUT) is earlier than s2's (later + TIMEOUT).
        assert_eq!(state.next_deadline(), Some(now + TIMEOUT));
    }

    #[test]
    fn timeout_only_evicts_expired_sessions_leaving_others_attention() {
        let now = Instant::now();
        let (state, _) = apply_event(&TabState::new(), "s1", HookEvent::Notify, now, TIMEOUT);
        let later = now + Duration::from_secs(300);
        let (state, _) = apply_event(&state, "s2", HookEvent::Notify, later, TIMEOUT);
        // Sweep at s1's deadline: s1 expires, s2 (refreshed later) does not.
        let (state, actions) = apply_timeout(&state, now + TIMEOUT + Duration::from_secs(1));
        assert!(actions.is_empty(), "s2 is still attention, the aggregate must not change");
        assert_eq!(state.attention_session_count(), 1);
        assert!(state.is_attention());
    }

    #[test]
    fn fresh_tab_state_has_no_deadline_and_is_idle() {
        let state = TabState::new();
        assert!(!state.is_attention());
        assert_eq!(state.next_deadline(), None);
    }
}
