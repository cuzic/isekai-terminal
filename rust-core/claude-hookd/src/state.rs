//! The pure decision core of `claude-hookd` (`ISEKAI_PIPE_DESIGN.md` §8
//! Epic Q): given the tab's current [`TabState`], a hook event for one
//! Claude Code session, and the current time, decides the next state and
//! which [`Action`]s (if any) the async driver should perform.
//!
//! Deliberately I/O-free and takes `now`/durations as parameters rather than
//! reading real time internally — the same "pure function, tested without
//! waiting on real timers or touching the network" shape as
//! `isekai-bootstrap-plan`'s `validate_jump_chain`. Every transition, including
//! both timeout paths, is exercised in this module's tests in microseconds,
//! not the real 10-minute/30-minute timeouts.
//!
//! State is keyed per **Claude Code session** (`session_id` from the hook
//! JSON payload — one tmux pane = one session), not per tab
//! (`$ISEKAI_CTL_SOCK`, shared by every pane in the same SSH connection).
//! Keying per tab instead of per session was the original (buggy) design:
//! resolving one pane's session would silently clear another pane's still-
//! pending attention, and vice versa (see the multi-session tests below,
//! `multiple_sessions_in_a_tab` — this module's test suite exists
//! specifically to pin the fix for that bug).
//!
//! A tab is one of three aggregate states, in ascending priority order —
//! [`Aggregate::Idle`] < [`Aggregate::Waiting`] < [`Aggregate::Attention`] —
//! since 2026-08 (see `main.rs`'s module docs for the history: `Stop` alone
//! can't distinguish "a human is genuinely needed" from "Claude is waiting on
//! a background task that will auto-resume it", so an ambiguous `Stop`
//! becomes [`Pending::Deferred`] — visibly different from full `Idle` (in
//! case the guess is wrong, the user still sees *a* color change rather than
//! nothing for up to [`Pending::Deferred`]'s bound) but distinct from the
//! unambiguous, popup-worthy [`Pending::Attention`]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One hook event, already reduced from Claude Code's hook JSON payload
/// (`hook_event_name`/`tool_name`/`background_tasks`) by the caller — this
/// module knows nothing about Claude Code's wire format, only "a session
/// needs attention", "a session is ambiguous but plausibly self-resolving",
/// or "a session is resolved".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookEvent {
    /// `Notification`/`StopFailure`/`PermissionRequest`, a `Stop` with no
    /// relevant `background_tasks` entry, or `PreToolUse` matched to
    /// `AskUserQuestion` — an unambiguous "a human is needed" signal.
    Notify,
    /// A `Stop` whose `background_tasks` contains a `"shell"`/`"subagent"`
    /// entry still `running`/`pending` — ambiguous: plausibly just Claude
    /// waiting on that work to finish and auto-resume it, but not
    /// necessarily (the entry could be unrelated to why this turn ended, or
    /// long-lived past this module's bound — see [`Pending::Deferred`]).
    StopDeferred,
    /// `UserPromptSubmit`, or `PostToolUse` matched to `AskUserQuestion`.
    Resolve,
}

/// An effect the async driver should perform in response to a transition.
/// Never inferred from the *current* state alone — only ever returned
/// alongside the transition that caused it, so the driver never has to
/// re-derive "did the aggregate actually change" itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Every variant is deliberately `Set<Thing>` — "an OSC write the driver
// should perform," consistently named, not an accidental shared prefix.
#[allow(clippy::enum_variant_names)]
pub(crate) enum Action {
    /// The tab's aggregate state just became [`Aggregate::Attention`] from
    /// something lower ([`Aggregate::Idle`] or [`Aggregate::Waiting`]).
    /// Fired **once** per such transition, not on every debounce refresh or
    /// every additional session that joins an already-`Attention` tab — see
    /// this module's docs on why the popup isn't per-event.
    SetAttentionColorAndPopup,
    /// The tab's aggregate state just became [`Aggregate::Waiting`] from
    /// either side — [`Aggregate::Idle`] (a fresh ambiguous `Stop`) or down
    /// from [`Aggregate::Attention`] (the last `Attention` session cleared
    /// or timed out, but a `Deferred` one is still pending). No popup: this
    /// is a "something's happening, probably nothing you need to do" signal,
    /// not a "come look now" one.
    SetWaitingColor,
    /// The tab's aggregate state just became [`Aggregate::Idle`] — nothing
    /// pending at all.
    SetIdleColor,
}

/// The tab's aggregate color state, in ascending priority — the actual
/// color is `max` over every session's individual state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aggregate {
    Idle,
    Waiting,
    Attention,
}

/// One session's pending state: either genuinely `Attention`-worthy, or
/// `Deferred` — an ambiguous `Stop` that gets the benefit of the doubt until
/// `deadline`, at which point [`apply_timeout`] promotes it to `Attention`
/// rather than ever discarding it silently (a wrong guess must degrade to
/// the safe, over-notifying default eventually, never mask a real
/// attention-needed state forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Attention(Instant),
    Deferred(Instant),
}

impl Pending {
    fn deadline(self) -> Instant {
        match self {
            Pending::Attention(d) | Pending::Deferred(d) => d,
        }
    }
}

/// A tab's state: every Claude Code session currently in [`Pending::Attention`]
/// or [`Pending::Deferred`], each with its own deadline. A session absent
/// from this map is implicitly idle — there is no need to track idle
/// sessions, so this map is empty exactly when the tab's aggregate color is
/// [`Aggregate::Idle`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TabState {
    sessions: HashMap<String, Pending>,
}

impl TabState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn aggregate(&self) -> Aggregate {
        if self.sessions.values().any(|p| matches!(p, Pending::Attention(_))) {
            Aggregate::Attention
        } else if !self.sessions.is_empty() {
            Aggregate::Waiting
        } else {
            Aggregate::Idle
        }
    }

    /// Whether the tab's aggregate color should currently be the attention
    /// color — true iff at least one session is `Attention`. `Deferred`
    /// sessions alone do **not** count (that's [`Aggregate::Waiting`], a
    /// distinct, lower-priority color). Test-only: production code
    /// (`daemon.rs`) only ever needs `aggregate()`'s transition, via
    /// `apply_event`/`apply_timeout`'s returned `Action`s, never this raw
    /// query.
    #[cfg(test)]
    pub(crate) fn is_attention(&self) -> bool {
        self.aggregate() == Aggregate::Attention
    }

    /// The earliest pending deadline across every session, `Attention` or
    /// `Deferred`, or `None` if the tab is fully idle. The async driver
    /// sleeps until this instant (via `tokio::select!` racing new events,
    /// per the design doc) rather than polling — it must wake for either an
    /// `Attention` eviction or a `Deferred` promotion, so both variants'
    /// deadlines are in play.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.sessions.values().map(|p| p.deadline()).min()
    }

    #[cfg(test)]
    pub(crate) fn attention_session_count(&self) -> usize {
        self.sessions.values().filter(|p| matches!(p, Pending::Attention(_))).count()
    }

    #[cfg(test)]
    fn deferred_session_count(&self) -> usize {
        self.sessions.values().filter(|p| matches!(p, Pending::Deferred(_))).count()
    }

    /// This tab's state as JSON, for external consumers (`hooks.rs`'s
    /// command hooks) that have no access to this crate's Rust types.
    /// Deliberately a bespoke conversion rather than `#[derive(Serialize)]`
    /// on `TabState`/`Pending` directly: `Instant` has no portable
    /// wall-clock meaning, so each deadline is expressed as milliseconds
    /// remaining from `now` instead of serialized as-is. `aggregate_name` is
    /// threaded in by the caller (`daemon.rs::execute_actions` already knows
    /// it from the `Action` variant being handled) rather than recomputed
    /// here via `aggregate()`, so this can never disagree with the
    /// transition that triggered the call.
    pub(crate) fn to_hook_json(&self, aggregate_name: &str, now: Instant) -> serde_json::Value {
        let sessions: serde_json::Map<String, serde_json::Value> = self
            .sessions
            .iter()
            .map(|(session_id, pending)| {
                let (kind, deadline) = match pending {
                    Pending::Attention(d) => ("attention", d),
                    Pending::Deferred(d) => ("deferred", d),
                };
                let remaining_ms = deadline.saturating_duration_since(now).as_millis() as u64;
                (session_id.clone(), serde_json::json!({ "kind": kind, "deadline_ms_remaining": remaining_ms }))
            })
            .collect();
        serde_json::json!({ "aggregate": aggregate_name, "sessions": sessions })
    }
}

/// Applies one hook event for `session_id` and returns the new state plus
/// whatever `Action`s the aggregate transition requires. `attention_timeout`
/// governs both a fresh `Notify` and a `Deferred` session's promotion (via
/// [`apply_timeout`]) to `Attention`; `max_deferral` bounds how long a
/// `StopDeferred` session is given the benefit of the doubt.
pub(crate) fn apply_event(
    state: &TabState,
    session_id: &str,
    event: HookEvent,
    now: Instant,
    attention_timeout: Duration,
    max_deferral: Duration,
) -> (TabState, Vec<Action>) {
    let prev = state.aggregate();
    let mut next = state.clone();
    match event {
        HookEvent::Notify => {
            // Also covers the debounce case (session already `Attention` or
            // `Deferred`): `insert` on an existing key just replaces it,
            // which is exactly "refresh the timeout" (and, for a `Deferred`
            // session, correctly promotes it — an unambiguous `Notify`
            // always outranks an earlier ambiguous guess).
            next.sessions.insert(session_id.to_string(), Pending::Attention(now + attention_timeout));
        }
        HookEvent::StopDeferred => {
            // A `StopDeferred` must never *downgrade* a session already
            // genuinely `Attention` — an ambiguous signal can't override an
            // unambiguous one. And it must never refresh an existing
            // `Deferred` session's deadline (`or_insert`, not `insert`):
            // this is the actual fix for the repeated "still waiting…"
            // pattern (no tool calls in between, so every one of those
            // turns' `Stop`s is `StopDeferred`) — each rides the *original*
            // deadline rather than pushing it out on every occurrence, which
            // is what makes `max_deferral` an actual bound rather than a
            // window that resets forever as long as the pattern continues.
            // (It still does not bound *elapsed wall-clock time since the
            // session last did anything at all* — any intervening real
            // activity, e.g. a `PostToolUse`→`Resolve`, clears the entry
            // outright, and the next `StopDeferred` after that starts a
            // fresh window. That's deliberate: activity is itself evidence
            // the session isn't wedged, unlike a silent run of bare `Stop`s.)
            let already_attention = matches!(next.sessions.get(session_id), Some(Pending::Attention(_)));
            if !already_attention {
                next.sessions.entry(session_id.to_string()).or_insert(Pending::Deferred(now + max_deferral));
            }
        }
        HookEvent::Resolve => {
            // A `Resolve` for a session that was never pending (or already
            // resolved/timed out) is a no-op removal — deliberately not an
            // error, since `PostToolUse(AskUserQuestion)` firing without a
            // preceding `PreToolUse` Notify (e.g. daemon restarted mid-
            // question) must not panic or otherwise misbehave. Removes a
            // `Deferred` session just as readily as an `Attention` one: real
            // activity (a submitted prompt, a completed tool call) is
            // evidence the session isn't wedged, regardless of which
            // pending state it was in.
            next.sessions.remove(session_id);
        }
    }
    let next_agg = next.aggregate();
    (next.clone(), actions_for_transition(prev, next_agg))
}

/// Sweeps every session whose deadline has passed as of `now`: an expired
/// `Attention` session reverts to fully idle (removed from the map); an
/// expired `Deferred` session is promoted to `Attention` (with a fresh
/// `now + attention_timeout` deadline) rather than removed — the
/// self-correction path for a `StopDeferred` guess that turned out to be
/// wrong, or simply outlived its bound. Both kinds of expiry are handled in
/// one sweep so a single wake-up (the driver sleeps until
/// [`TabState::next_deadline`], which already covers both variants) can't
/// miss one kind while handling the other. Returns the new state plus
/// whatever `Action`s the resulting aggregate transition requires — a pure
/// sibling of [`apply_event`] for the "nothing happened, but time did" case.
pub(crate) fn apply_timeout(state: &TabState, now: Instant, attention_timeout: Duration) -> (TabState, Vec<Action>) {
    let prev = state.aggregate();
    let mut next = state.clone();
    next.sessions.retain(|_, p| !matches!(p, Pending::Attention(deadline) if *deadline <= now));
    for pending in next.sessions.values_mut() {
        if let Pending::Deferred(deadline) = *pending {
            if deadline <= now {
                *pending = Pending::Attention(now + attention_timeout);
            }
        }
    }
    let next_agg = next.aggregate();
    (next.clone(), actions_for_transition(prev, next_agg))
}

fn actions_for_transition(prev: Aggregate, next: Aggregate) -> Vec<Action> {
    use Aggregate::*;
    match (prev, next) {
        (Idle, Waiting) | (Attention, Waiting) => vec![Action::SetWaitingColor],
        (Idle, Attention) | (Waiting, Attention) => vec![Action::SetAttentionColorAndPopup],
        (Attention, Idle) | (Waiting, Idle) => vec![Action::SetIdleColor],
        // Same level on both sides: a refresh, a debounce, one of several
        // pending sessions changing while the aggregate stays put, or a
        // `StopDeferred` that was a no-op (already `Attention`) — none of
        // these are visible to the driver, by design (see `Action`'s docs).
        (Idle, Idle) | (Waiting, Waiting) | (Attention, Attention) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTENTION_TIMEOUT: Duration = Duration::from_secs(600);
    const MAX_DEFERRAL: Duration = Duration::from_secs(1800);

    fn notify(state: &TabState, session_id: &str, now: Instant) -> (TabState, Vec<Action>) {
        apply_event(state, session_id, HookEvent::Notify, now, ATTENTION_TIMEOUT, MAX_DEFERRAL)
    }

    fn stop_deferred(state: &TabState, session_id: &str, now: Instant) -> (TabState, Vec<Action>) {
        apply_event(state, session_id, HookEvent::StopDeferred, now, ATTENTION_TIMEOUT, MAX_DEFERRAL)
    }

    fn resolve(state: &TabState, session_id: &str, now: Instant) -> (TabState, Vec<Action>) {
        apply_event(state, session_id, HookEvent::Resolve, now, ATTENTION_TIMEOUT, MAX_DEFERRAL)
    }

    fn timeout(state: &TabState, now: Instant) -> (TabState, Vec<Action>) {
        apply_timeout(state, now, ATTENTION_TIMEOUT)
    }

    #[test]
    fn first_notify_enters_attention_and_fires_popup() {
        let now = Instant::now();
        let (state, actions) = notify(&TabState::new(), "s1", now);
        assert!(state.is_attention());
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup]);
        assert_eq!(state.next_deadline(), Some(now + ATTENTION_TIMEOUT));
    }

    #[test]
    fn second_notify_for_the_same_session_debounces_without_reissuing_actions() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let later = now + Duration::from_secs(60);
        let (state, actions) = notify(&state, "s1", later);
        assert!(actions.is_empty(), "debounce refresh must not resend the popup/color");
        // The deadline moved forward — this is the actual debounce effect.
        assert_eq!(state.next_deadline(), Some(later + ATTENTION_TIMEOUT));
    }

    #[test]
    fn resolve_of_the_only_attention_session_returns_to_idle() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let (state, actions) = resolve(&state, "s1", now);
        assert!(!state.is_attention());
        assert_eq!(actions, vec![Action::SetIdleColor]);
    }

    #[test]
    fn resolve_of_a_session_that_was_never_attention_is_a_silent_no_op() {
        let now = Instant::now();
        let (state, actions) = resolve(&TabState::new(), "ghost", now);
        assert!(!state.is_attention());
        assert!(actions.is_empty());
    }

    #[test]
    fn timeout_before_deadline_does_nothing() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let (state, actions) = timeout(&state, now + Duration::from_secs(1));
        assert!(state.is_attention());
        assert!(actions.is_empty());
    }

    #[test]
    fn timeout_after_deadline_returns_to_idle() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let (state, actions) = timeout(&state, now + ATTENTION_TIMEOUT + Duration::from_secs(1));
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
        let (state, actions) = notify(&TabState::new(), "pane-a", now);
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup]);

        // Pane B's Stop fires while A is still pending — no second popup,
        // the tab was already attention (this is the debounce-suppression
        // half of the bug: a naive "assert PreToolUse->PostToolUse pairing
        // only" design would have wrongly treated this as A's own debounce).
        let (state, actions) = notify(&state, "pane-b", now);
        assert!(actions.is_empty());
        assert_eq!(state.attention_session_count(), 2);

        // The user answers pane B first. Pane A is still blocked, so the
        // tab must stay in the attention color — this is the bug's other
        // half: a per-tab (not per-session) design would have wrongly
        // reverted to idle here, hiding A's still-pending question.
        let (state, actions) = resolve(&state, "pane-b", now);
        assert!(actions.is_empty(), "pane A is still attention; the tab must not revert to idle");
        assert!(state.is_attention());

        // Now pane A resolves too — only *now* does the tab revert.
        let (state, actions) = resolve(&state, "pane-a", now);
        assert_eq!(actions, vec![Action::SetIdleColor]);
        assert!(!state.is_attention());
    }

    #[test]
    fn next_deadline_is_the_earliest_across_sessions() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let later = now + Duration::from_secs(30);
        let (state, _) = notify(&state, "s2", later);
        // s1's deadline (now + TIMEOUT) is earlier than s2's (later + TIMEOUT).
        assert_eq!(state.next_deadline(), Some(now + ATTENTION_TIMEOUT));
    }

    #[test]
    fn timeout_only_evicts_expired_sessions_leaving_others_attention() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let later = now + Duration::from_secs(300);
        let (state, _) = notify(&state, "s2", later);
        // Sweep at s1's deadline: s1 expires, s2 (refreshed later) does not.
        let (state, actions) = timeout(&state, now + ATTENTION_TIMEOUT + Duration::from_secs(1));
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

    #[test]
    fn stop_deferred_on_a_fresh_session_enters_waiting_without_a_popup() {
        let now = Instant::now();
        let (state, actions) = stop_deferred(&TabState::new(), "s1", now);
        assert!(!state.is_attention(), "Deferred alone must not read as attention");
        assert_eq!(state.deferred_session_count(), 1);
        assert_eq!(actions, vec![Action::SetWaitingColor]);
        assert_eq!(state.next_deadline(), Some(now + MAX_DEFERRAL));
    }

    /// The actual fix for the repeated "still waiting…" pattern: each
    /// subsequent `StopDeferred` for the same session (no tool call, hence
    /// no `Resolve`, in between) must not push the deadline out further.
    #[test]
    fn repeated_stop_deferred_for_the_same_session_does_not_refresh_the_deadline() {
        let now = Instant::now();
        let (state, actions) = stop_deferred(&TabState::new(), "s1", now);
        assert_eq!(actions, vec![Action::SetWaitingColor]);
        let later = now + Duration::from_secs(300);
        let (state, actions) = stop_deferred(&state, "s1", later);
        assert!(actions.is_empty(), "still Waiting, no visible change");
        assert_eq!(state.next_deadline(), Some(now + MAX_DEFERRAL), "the deadline must not have moved");
    }

    #[test]
    fn stop_deferred_on_an_already_attention_session_does_not_downgrade_it() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let (state, actions) = stop_deferred(&state, "s1", now + Duration::from_secs(1));
        assert!(actions.is_empty());
        assert!(state.is_attention(), "an ambiguous StopDeferred must never override a real Attention");
        assert_eq!(state.deferred_session_count(), 0);
    }

    #[test]
    fn resolve_clears_a_deferred_session_back_to_idle() {
        let now = Instant::now();
        let (state, _) = stop_deferred(&TabState::new(), "s1", now);
        let (state, actions) = resolve(&state, "s1", now + Duration::from_secs(5));
        assert_eq!(actions, vec![Action::SetIdleColor]);
        assert_eq!(state.next_deadline(), None);
    }

    /// The self-correction path: a `Deferred` session that outlives
    /// `max_deferral` gets promoted to `Attention` instead of being
    /// silently forgotten — a wrong "probably just background work" guess
    /// must eventually degrade to the safe, over-notifying default.
    #[test]
    fn deferred_session_promotes_to_attention_after_max_deferral_elapses() {
        let now = Instant::now();
        let (state, _) = stop_deferred(&TabState::new(), "s1", now);
        let (state, actions) = timeout(&state, now + MAX_DEFERRAL + Duration::from_secs(1));
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup]);
        assert!(state.is_attention());
        assert_eq!(state.deferred_session_count(), 0);
    }

    #[test]
    fn deferred_session_before_max_deferral_is_untouched_by_timeout() {
        let now = Instant::now();
        let (state, _) = stop_deferred(&TabState::new(), "s1", now);
        let (state, actions) = timeout(&state, now + Duration::from_secs(5));
        assert!(actions.is_empty());
        assert_eq!(state.deferred_session_count(), 1);
    }

    /// Aggregate priority: one session `Attention` and another `Deferred`
    /// must still read as `Attention` (the max over sessions), and losing
    /// the `Attention` one (but not the `Deferred` one) must downgrade to
    /// `Waiting`, not jump straight to `Idle`.
    #[test]
    fn attention_outranks_deferred_in_the_aggregate_and_downgrades_to_waiting_not_idle() {
        let now = Instant::now();
        let (state, _) = stop_deferred(&TabState::new(), "pane-a", now);
        let (state, actions) = notify(&state, "pane-b", now);
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup], "Attention must win over Deferred");
        assert!(state.is_attention());

        let (state, actions) = resolve(&state, "pane-b", now + Duration::from_secs(1));
        assert_eq!(actions, vec![Action::SetWaitingColor], "pane-a is still Deferred; must not drop to Idle");
        assert!(!state.is_attention());
        assert_eq!(state.deferred_session_count(), 1);
    }

    #[test]
    fn waiting_to_attention_transition_fires_the_popup_same_as_idle_to_attention() {
        let now = Instant::now();
        let (state, _) = stop_deferred(&TabState::new(), "s1", now);
        // A second, unambiguous Notify on the *same* session promotes it.
        let (state, actions) = notify(&state, "s1", now + Duration::from_secs(1));
        assert_eq!(actions, vec![Action::SetAttentionColorAndPopup]);
        assert!(state.is_attention());
    }

    #[test]
    fn to_hook_json_of_an_idle_tab_has_no_sessions() {
        let now = Instant::now();
        let json = TabState::new().to_hook_json("idle", now);
        assert_eq!(json, serde_json::json!({ "aggregate": "idle", "sessions": {} }));
    }

    #[test]
    fn to_hook_json_reports_kind_and_remaining_ms_per_session() {
        let now = Instant::now();
        let (state, _) = notify(&TabState::new(), "s1", now);
        let json = state.to_hook_json("attention", now);
        assert_eq!(json["aggregate"], "attention");
        assert_eq!(json["sessions"]["s1"]["kind"], "attention");
        assert_eq!(json["sessions"]["s1"]["deadline_ms_remaining"], ATTENTION_TIMEOUT.as_millis() as u64);
    }

    #[test]
    fn to_hook_json_distinguishes_deferred_from_attention_and_uses_aggregate_names_verbatim() {
        let now = Instant::now();
        let (state, _) = stop_deferred(&TabState::new(), "pane-a", now);
        // `aggregate_name` is caller-supplied (see this method's docs on why),
        // so this pins that the string is passed through unmodified rather
        // than recomputed from `self.aggregate()` — the two happen to agree
        // here (a lone `Deferred` session aggregates to `Waiting`), but the
        // point of the parameter is that this method never has to know that.
        let json = state.to_hook_json("waiting", now);
        assert_eq!(json["aggregate"], "waiting");
        assert_eq!(json["sessions"]["pane-a"]["kind"], "deferred");
        assert_eq!(json["sessions"]["pane-a"]["deadline_ms_remaining"], MAX_DEFERRAL.as_millis() as u64);
    }
}
