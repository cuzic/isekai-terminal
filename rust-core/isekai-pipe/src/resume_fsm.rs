//! `isekai-pipe connect`のresume判断の純粋reducer [`ResumePlanner`]
//! (docs/adr/0019-functional-core-effects.md §6 Step 5)。
//!
//! 切断を検知してから「どの経路へいつ再接続を試みるか」「いつ諦めるか」を決める判断を、
//! shell(`resume_loop.rs`の`run_resume_loop`/`resume_with_backoff_until_deadline`)から
//! ここへ移した。このファイルの判断は過去に何度も直し直されている
//! (UnknownSessionのgive-up方針: cc5fb926 → 71292e68 → dbb80d56、
//! BUSY_OTHER_SESSIONの再試行期限: 75d08a39 → fd32ce11 → 3d5e0da5、resume window超過で
//! `Ok`を返していた03224b11、通知猶予a266f1f3、jitter欠落857f6ae6 D-4)。それぞれの不変条件を
//! このモジュールのproptestで固定する。
//!
//! 形はADR §2.1の標準形(`apply(&mut self, Event) -> Vec<Cmd>`):
//!
//! - 時刻はshellがEventに刻印する`now: Millis`としてのみ入り、差は`Millis::saturating_sub`
//!   でのみ計算する(§2.2。`now`が逆行しても期限切れ/give-upを誤発火させない)。
//! - タイマーとダイヤルは[`ResumeToken`]付きのCmd(`Backoff`/`Dial`)で、結果のEventは
//!   同じtokenを運ぶ。reducerは現在待っているtokenと一致しないEventを無視する(§2.2 stale-guard)。
//! - jitterの乱数はshellが`jitter_seed`としてEventに載せ、`BackoffPolicy::next_delay`へ渡す
//!   (§2.4-5、Step 6)。
//! - Event/Cmdは秘密情報(session secret・接続先の証明書等)を一切持たない(§3-3)。接続先は
//!   [`DialPath`]という不透明な選択肢で指し、実体の`RelayTarget`はshellが持つ。
//!
//! 後続のL1合成proptest(docs/adr/0021-deterministic-network-simulation-l1.md §4.6)が`ServeAggregate`
//! (server)とこのreducer(client)を同じ`Millis`時計で結ぶことを想定し、client側の期限
//! (`GiveUp`の`resume_window`、`Backoff`の`after`)をすべてCmdの値として観測できるようにしてある。
//!
//! 範囲外(shellに残したもの): EOF-latch(`should_give_up_without_resuming`、`PumpFailure`に
//! `anyhow::Error`が乗るため)、stdio、表示・通知・telemetryの実I/O(判断はここ、実行はshell)。
// 純粋モジュール(`pure_modules.toml`登録、docs/adr/0019-functional-core-effects.md §2.3)。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

use isekai_protocol::Millis;
use isekai_transport::BackoffPolicy;

/// `crate::DEFAULT_RESUME_WINDOW`(`main.rs`)と同じ値。純粋モジュールはcrate rootを参照
/// できない(§2.3)ので、同じ定義元(`isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS`)から
/// 直接導出する(一致は`resume_loop.rs`の`resume_window_for_zero_falls_back_..`テストが固定)。
const DEFAULT_RESUME_WINDOW: Duration = Duration::from_secs(isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS);

/// `t + d`(飽和加算)。`Millis`は`isekai-protocol`の値型で加算を持たないので、ここで定義する。
fn millis_after(t: Millis, d: Duration) -> Millis {
    Millis(t.0.saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
}

// ── 方針定数(`resume_loop.rs`から移設。docは移設前のまま) ───────────────

/// `jitter: 0.25`(±25%、ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-4): スリープ
/// 復帰やネットワーク瞬断は「複数タブが同時に同じイベントを見る」典型例
/// であり、ジッター無しの純粋な指数バックオフでは全タブが同一グリッド上で
/// 同期再試行し、サーバー側のresume競合(D-2)を確実に踏みにいく。
pub(crate) const RESUME_BACKOFF: BackoffPolicy = BackoffPolicy {
    initial: Duration::from_millis(500),
    max: Duration::from_secs(10),
    jitter: 0.25,
};
/// How many consecutive failed bare-redial attempts against the *original*
/// STUN peer address, within one disconnect episode, before switching to the
/// cross-family relay fallback (if one exists) — independent of, and usually
/// reached much sooner than, `UNKNOWN_SESSION_CONFIRM_THRESHOLD` (that streak
/// only increments on a specific `UnknownSession` *rejection*; this counter
/// increments on *any* attempt failure, since a STUN bare redial against a
/// peer address the client can no longer reach typically fails as a mux/QUIC
/// dial error, never even reaching a point where the server could reject it).
/// At `RESUME_BACKOFF`'s schedule (500ms, 1s, 2s, 4s, 8s, capped at 10s) the
/// cumulative *wait between* attempts is roughly 15s — **but that is not
/// when the switch actually fires** (opus review round 5 on this ADR's
/// implementation): each attempt's own `reconnect_and_resume` can itself
/// cost up to two separate `TRANSPORT_STEP_TIMEOUT`s (its `connect` step
/// and its `request_resume` step are timed independently, ~15s each — see
/// `isekai_transport::resume::TRANSPORT_STEP_TIMEOUT`'s own docs), so the
/// count alone can take up to ~90s (15s/attempt) or ~165s (30s/attempt) of
/// wall-clock time to reach this many failures — the second figure already
/// exceeds `STUN_RESUME_GIVE_UP_WINDOW` (120s), meaning the count-based
/// trigger could silently *never* fire before the deadline it's supposed to
/// preempt. `should_switch_to_cross_family` (near `cross_family_switch_
/// budget`) closes this by also switching once the *remaining* time before
/// the deadline stops being enough for even one cross-family probe,
/// independent of this attempt count — see that function's own docs.
///
/// Deliberately no preempt/ping-pong latch here (docs/adr/0006-stun-reestablish-continuity.md
/// §3.2 task 8) — round 2 review concluded cross-family resume runs as a
/// single sequential loop with no second concurrent reconnect driver, so
/// there's nothing to latch against yet; build one only if real-world
/// measurement shows otherwise. The server's own PREEMPT_WAIT_TIMEOUT
/// (engine/mod.rs, 2s) adds at most 2s of latency to whichever attempt races
/// it, comfortably inside this switch's own bounded windows above.
pub(crate) const STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS: u32 = 5;
/// How long, measured from the *moment of the switch itself* (not from
/// `disconnected_at` — see the R1 note below), `resume_with_backoff_until_
/// deadline` keeps retrying the cross-family relay target *before that
/// target has ever succeeded* in this disconnect episode.
///
/// This is deliberately **not** the same as the relay-grace-based deadline
/// (`None`/multi-day) that `run_resume_loop` installs for later episodes
/// once the switch has actually succeeded once (docs/adr/0006-stun-reestablish-continuity.md
/// §3.2 task 7's "成功した後" wording, and its own separate task 4 bullet
/// requiring a *bounded* first attempt) — the two are easy to conflate
/// because both are implemented as `ResumeDeadlinePolicy::max_resume_window`
/// (opus review round on this ADR's implementation, finding C1: an earlier
/// cut of this code installed the multi-day deadline immediately upon
/// switching, before the cross-family target had ever answered, which — if
/// `cached_relay_addr` turned out to be unreachable from the client's new
/// network, exactly the unverified assumption §3.3 calls out — made
/// `isekai-pipe connect` hang for up to `DEFAULT_RESUME_GRACE_SECS` instead
/// of ever returning control to `isekai-ssh`'s wrapper-level
/// `lightweight_retries`/`redeploy_gate` escalation that `always-connects.md`
/// depends on).
///
/// **R1** (opus review round 2 on this ADR's implementation): the switch
/// call site adds this to *elapsed time since `disconnected_at`* rather than
/// anchoring the whole window to `disconnected_at` directly, because the
/// switch itself can already be tens of seconds into the episode by the
/// time it fires — `switch_attempts_before_cross_family` real attempts
/// against the *original* STUN target, each up to two separate
/// `isekai_transport::resume::TRANSPORT_STEP_TIMEOUT`s (15s each, for its
/// `connect` and `request_resume` steps — see `STUN_TO_CROSS_FAMILY_SWITCH_
/// ATTEMPTS`'s own docs for the full corrected cost model, opus review
/// round 5), not just the `RESUME_BACKOFF` waits between them. Anchoring this constant to
/// `disconnected_at` instead left as little as zero of it for the
/// cross-family target on a slow-to-fail STUN peer, defeating task 4's
/// "short *bounded retry*" (up to ~`UNKNOWN_SESSION_CONFIRM_THRESHOLD`
/// attempts) requirement — see the call site's own `.max(
/// UNKNOWN_SESSION_MIN_ELAPSED_FLOOR)`, which keeps task 4's *other* half
/// (never give up before 30s since `disconnected_at`) true even when the
/// switch happens quickly. That `.max()` is a no-op against *this*
/// constant's current value (45s > 30s, so the sum term always wins) —
/// kept anyway as a guard against a future tuning of `CROSS_FAMILY_SWITCH_
/// DEADLINE` below 30s silently reopening the gap it exists to close
/// (confirmed intentional, opus review round 3 on this ADR's
/// implementation).
///
/// This is a budget on the cross-family target *alone*, from the moment of
/// the switch — not a promise that the whole episode (STUN attempts plus
/// this) stays under `STUN_RESUME_GIVE_UP_WINDOW` (120s) end to end
/// (`/code-review` finding on this ADR's implementation, correcting an
/// earlier version of this doc that claimed exactly that). In the realistic
/// worst case (all `switch_attempts_before_cross_family` STUN attempts each
/// burning up to *two* separate `TRANSPORT_STEP_TIMEOUT`s before failing —
/// their `connect` and `request_resume` steps are timed independently, not
/// jointly — see `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS`'s own docs, updated
/// after opus review round 5 caught this same undercount here), the
/// count-based switch can be up to ~90s (one `TRANSPORT_STEP_TIMEOUT`/attempt)
/// or ~165s (two/attempt) into the episode by the time it *would* fire —
/// the second figure already past `STUN_RESUME_GIVE_UP_WINDOW`. This
/// constant alone doesn't reach that trigger in time; the deadline-aware
/// half of `should_switch_to_cross_family` (near `cross_family_switch_
/// budget`, below) is what actually keeps the switch from silently never
/// firing in that band — see its own docs. Either way, `isekai-ssh`'s
/// wrapper-level `lightweight_retries`/`redeploy_gate` escalation still
/// takes over once this function finally returns `Err` — always-connects.md
/// is about *eventual* automatic recovery, not a hard latency SLA.
pub(crate) const CROSS_FAMILY_SWITCH_DEADLINE: Duration = Duration::from_secs(45);
/// The smallest remaining slice of the current deadline in which switching
/// to the cross-family relay target can still buy anything: one
/// `RESUME_BACKOFF` first delay (500ms, +25% jitter, rounded up to a whole
/// second here) plus one `isekai_transport::resume::TRANSPORT_STEP_TIMEOUT`-
/// bounded QUIC connect step. Derived from that constant directly (not
/// hand-mirrored as a second literal) — `/code-review` finding on this
/// ADR's implementation: a hand-mirrored copy is exactly the kind of
/// cross-crate drift this ADR's own review rounds already caught once for
/// this same 15s-vs-30s distinction (opus review round 5).
///
/// Deliberately *not* the ~30s worst case of a whole `reconnect_and_resume`
/// (its `connect` and `request_resume` steps are each bounded by
/// `TRANSPORT_STEP_TIMEOUT` *separately*, not jointly): demanding 31s of
/// headroom would suppress the switch across exactly the moderate
/// `#@isekai resume-grace` band where it matters most, and the case this
/// ADR exists for (a `cached_relay_addr` the client's new network cannot
/// reach at all) is decided in the *connect* step alone — a probe that only
/// gets this far still yields a real relay-reachability verdict (opus
/// review round 5 on this ADR's implementation).
pub(crate) const CROSS_FAMILY_MIN_PROBE_BUDGET: Duration = Duration::from_secs(isekai_transport::resume::TRANSPORT_STEP_TIMEOUT.as_secs() + 1);
/// How long a disconnect stays silent before `print_reconnect_status` (shell)
/// actually prints anything. Matches trzsz-ssh's own
/// `kDefaultUdpReconnectTimeout` (`tssh/udp.go`) — `tssh` polls liveness
/// every ~1s but only calls its `notifyConnectionLost()` once elapsed time
/// since the last active moment exceeds this same 15s, so a blip shorter
/// than that produces no visible output at all on that client. Before this
/// existed, `run_resume_loop` printed a status line the instant a disconnect
/// was *detected* (see the call site's history) — every brief Wi-Fi/cellular
/// handoff surfaced a "connection lost" message that a same-length outage
/// on `tssh` never showed, even though both sides were transparently
/// recovering within their (much longer) resume windows the whole time.
pub(crate) const RECONNECT_NOTIFY_GRACE: Duration = Duration::from_secs(15);
/// Deliberately **not** derived from `resume_window_for`/`resume-grace`
/// (unlike a same-process resume loop's own deadline) — even though a
/// `BUSY_OTHER_SESSION` reject on the very first connect most often means
/// *this same client's* previous session is still parked on the remote
/// helper (see `TransportError::is_busy_other_session`'s docs), waiting for
/// that park to clear on its own is no longer a sound thing to size against
/// `resume-grace`, now that the value is days long by default
/// (`isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS`'s docs). A fixed, short
/// window here is defense-in-depth for `isekai-pipe serve` deployments that
/// predate `ISEKAI_PIPE_DESIGN.md` §8's parked-session-preemption fix
/// (`engine/mod.rs::hello_with_parked_preemption`) — helper reuse
/// deliberately doesn't force those to redeploy (`reuse.rs`'s fingerprint
/// exclusion), so they can keep running the old, un-preempting behavior for
/// up to `--max-idle-lifetime` (30 days) after this fix ships. Against a
/// server that *does* have the fix, a legitimate retry here succeeds almost
/// immediately (the preemption is atomic, no real waiting involved), so
/// this window is never the limiting factor in the common case; against one
/// that doesn't, it turns what would otherwise be a silent multi-day hang
/// into a fast, visible failure instead.
pub(crate) const BUSY_OTHER_SESSION_RETRY_WINDOW: Duration = Duration::from_secs(180);
/// How many *consecutive* `UnknownSession` rejections
/// `resume_with_backoff_until_deadline` requires before treating the
/// session as genuinely, permanently gone — see `is_unknown_session_rejection`'s
/// docs for why a single occurrence isn't proof enough.
pub(crate) const UNKNOWN_SESSION_CONFIRM_THRESHOLD: u32 = 3;
/// Minimum time since disconnect that must have elapsed before
/// `UNKNOWN_SESSION_CONFIRM_THRESHOLD` consecutive rejections are trusted as
/// proof of permanent loss, required *in addition to* the streak count
/// above. At `RESUME_BACKOFF`'s schedule (500ms, 1s, 2s, ...) 3 consecutive
/// attempts land around t≈3.5s — comfortably inside `isekai-pipe serve
/// --idle-timeout`'s default (15s) worst case for the "not-yet-parked"
/// race `is_unknown_session_rejection` describes: a "break-before-make"
/// roam (Wi-Fi drop, AP switch, airplane mode) means the client's
/// `quic_write.reset(0)` on the *old* path never reaches the server, so the
/// server can only notice the old connection died via its own QUIC idle
/// timeout, not the explicit reset — the fast path this streak was tuned
/// around. Without this floor, `UNKNOWN_SESSION_CONFIRM_THRESHOLD` alone
/// would misfire and kill exactly the roaming reconnects this project's
/// resumable transport exists to survive (`CLAUDE.md`'s differentiator:
/// "QUIC接続耐性(ローミング...)"). 30s is double the idle-timeout default,
/// leaving margin without meaningfully eating into the (now days-long)
/// deadline a truly-dead session would otherwise be retried against.
pub(crate) const UNKNOWN_SESSION_MIN_ELAPSED_FLOOR: Duration = Duration::from_secs(30);


// ── 純粋helper(`resume_loop.rs`から移設) ────────────────────────────

/// The server clamps our request to its own configured max (or applies its
/// own default when we requested `0`) and echoes back what it actually
/// granted — that, not our own request, is the real deadline: the server
/// will have already discarded the parked session past this point
/// regardless of how long we keep retrying (`ISEKAI_PIPE_DESIGN.md`).
///
/// `0` itself is treated as "no real value was ever learned" rather than a
/// literal zero-second window: `isekai-transport::resume::finish_via_resume`
/// (the `MustResume` ambiguous-attach convergence path) has no ATTACH_HELLO
/// exchange to learn the server's actual grant from, and — even after that
/// function's own fix to fall back to the caller's originally *requested*
/// grace period instead of hardcoding `0` — a caller that itself requested
/// `0` (isekai-ssh/isekai-pipe connect's own "let the server pick its
/// default" convention) still produces `0` here. Without this fallback, any
/// session that ever passed through that convergence path would give up on
/// its very first subsequent disconnect instead of resuming at all (codex
/// review, quicmux-server-resume).
pub(crate) fn resume_window_for(effective_resume_grace_secs: u32) -> Duration {
    match effective_resume_grace_secs {
        0 => DEFAULT_RESUME_WINDOW,
        secs => Duration::from_secs(secs.into()),
    }
}

pub(crate) fn clamp_resume_window(resume_window: Duration, max_resume_window: Option<Duration>) -> Duration {
    max_resume_window.map(|max| resume_window.min(max)).unwrap_or(resume_window)
}

/// `run_resume_loop`'s own resume-window computation, factored out so it has
/// exactly one call site in production code and one in its test (round 3
/// code review, significant finding on the first cut of this test): a test
/// that merely *re-derived* `clamp_resume_window(resume_window_for(...), ...)`
/// inline, rather than calling the same function `run_resume_loop` calls,
/// couldn't actually catch a regression that dropped the clamp from
/// `run_resume_loop` itself — it would keep passing because it never
/// exercises that call site at all.
pub(crate) fn effective_resume_window(effective_resume_grace_secs: u32, max_resume_window: Option<Duration>) -> Duration {
    clamp_resume_window(resume_window_for(effective_resume_grace_secs), max_resume_window)
}

/// Pure decision core of the `UnknownSession` streak-tracking described on
/// `ResumePlanner::consecutive_unknown_session`'s docs, factored out so it
/// can be unit-tested without a real `AnyMuxFactory`/network dial (unlike
/// `resume_with_backoff_until_deadline` itself). Given the streak length
/// going into this attempt, whether this attempt's error was an
/// `UnknownSession` rejection, and how long it's been since the disconnect
/// that started this episode, returns the streak length coming out of it
/// and whether the caller should give up now — both the streak count *and*
/// `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` must be satisfied (see that
/// constant's docs for why the streak alone isn't enough).
pub(crate) fn update_unknown_session_streak(previous_streak: u32, is_unknown_session: bool, elapsed_since_disconnect: Duration) -> (u32, bool) {
    if !is_unknown_session {
        return (0, false);
    }
    let streak = previous_streak.saturating_add(1);
    let should_give_up = streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD && elapsed_since_disconnect >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR;
    (streak, should_give_up)
}

/// Whether a switch made *now* would still get at least one real probe in
/// before the current `deadline` — see [`CROSS_FAMILY_MIN_PROBE_BUDGET`].
///
/// Switching without this is worse than not switching: the very next thing
/// `resume_with_backoff_until_deadline` does is its loop-top deadline
/// give-up, which then records `continuity-lost` / `"relay-unreachable"`
/// for an episode that never sent a single packet to the relay — the same
/// ADR §6 denominator inflation N1 (opus review round 4) closed at the
/// *other* give-up site (the `UnknownSession`-streak one), reached here
/// through the residual gap round 4's own N2 recorded as harmless (opus
/// review round 5 on this ADR's implementation).
pub(crate) fn cross_family_probe_fits(remaining_before_deadline: Duration) -> bool {
    remaining_before_deadline >= CROSS_FAMILY_MIN_PROBE_BUDGET
}

/// The failure-count switch trigger (docs/adr/0006-stun-reestablish-continuity.md
/// §3.2 task 1's second disjunct), made deadline-aware.
///
/// The count alone can silently *never* fire whenever the episode's
/// deadline is shorter than what reaching `switch_attempts_before_cross_
/// family` failures actually takes — see `STUN_TO_CROSS_FAMILY_SWITCH_
/// ATTEMPTS`'s own docs (opus review round 5 on this ADR's implementation).
/// That is the same "silently degrades back into waiting out the whole
/// window" failure ADR §3.2 task 1 forbids, arrived at from the other side:
/// so this also switches once what remains of the window stops being any
/// bigger than the cross-family probe itself would want — at that point
/// another bare redial against the *original* target can only consume the
/// remaining window, while the fallback can still change the outcome.
///
/// Deliberately *lowers* the deadline's meaning here, never raises it — the
/// client must never retry past the server's own grant, which is also what
/// discards the parked session (`resume_window_for`'s own docs,
/// `engine/mod.rs`'s `max_parked`/`effective_resume_grace`) — so this
/// cannot be satisfied by widening `remaining_before_deadline` itself, only
/// by switching sooner within it.
pub(crate) fn should_switch_to_cross_family(stun_failures: u32, switch_attempts_before_cross_family: u32, remaining_before_deadline: Duration) -> bool {
    cross_family_probe_fits(remaining_before_deadline)
        && (stun_failures >= switch_attempts_before_cross_family || remaining_before_deadline <= CROSS_FAMILY_SWITCH_DEADLINE)
}

/// The four `ResumeDeadlinePolicy`-shaped values (now [`Episode`] fields) that change the moment
/// `resume_with_backoff_until_deadline` switches to the cross-family relay
/// target — computed once here (`/code-review` finding on this ADR's
/// implementation: the two call sites that trigger a switch used to
/// recompute all four inline, identically apart from the trigger's own log
/// message) so they can't drift from each other. See `CROSS_FAMILY_SWITCH_
/// DEADLINE`'s own doc for why the budget is anchored to *now*, not to
/// `disconnected_at`.
struct CrossFamilySwitchBudget {
    max_resume_window: Option<Duration>,
    resume_window: Duration,
    deadline: Millis,
}

/// `now`は呼び出し元(reducer)が受け取ったEventの刻印。差は`saturating_sub`でのみ取る
/// (時刻が`disconnected_at`より前へ逆行していても経過0として扱う、ADR §2.2)。
fn cross_family_switch_budget(disconnected_at: Millis, now: Millis, effective_resume_grace_secs: u32) -> CrossFamilySwitchBudget {
    let elapsed_since_disconnect = now.saturating_sub(disconnected_at);
    let max_resume_window = Some((elapsed_since_disconnect + CROSS_FAMILY_SWITCH_DEADLINE).max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR));
    let resume_window = effective_resume_window(effective_resume_grace_secs, max_resume_window);
    CrossFamilySwitchBudget { max_resume_window, resume_window, deadline: millis_after(disconnected_at, resume_window) }
}

/// Whether [`ResumeCmd::ShowReconnecting`]/`announce` should surface anything
/// yet — the `RECONNECT_NOTIFY_GRACE` boundary (a266f1f3 part 1: a reconnect
/// notice used to be shown without the 15s grace). `now`が`disconnected_at`より
/// 前へ逆行していても経過0(=未到達)として扱う。
pub(crate) fn reconnect_notify_due(disconnected_at: Millis, now: Millis) -> bool {
    now.saturating_sub(disconnected_at) >= RECONNECT_NOTIFY_GRACE
}

// ── BUSY_OTHER_SESSIONの再試行期限(75d08a39 → fd32ce11 → 3d5e0da5) ─────────

/// `retry_while_busy_other_session`(shell)の判断部分。期限は**開始時刻 + `window`だけ**から
/// 決まり、resume graceには一切依存しない(3d5e0da5、`BUSY_OTHER_SESSION_RETRY_WINDOW`のdoc)。
/// コンストラクタがgraceを受け取らないこと自体がその不変条件の構造的な表現。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BusyOtherSessionRetry {
    deadline: Millis,
    attempt: u32,
}

/// [`BusyOtherSessionRetry::on_failure`]の判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusyRetryDecision {
    /// 失敗をそのまま呼び出し元へ返す(BUSY以外の失敗、または期限到達)。
    GiveUp,
    /// `after`待ってから再試行する。`after`は残り期限を超えない。
    RetryAfter { after: Duration },
}

impl BusyOtherSessionRetry {
    pub(crate) fn start(now: Millis, window: Duration) -> Self {
        Self { deadline: millis_after(now, window), attempt: 0 }
    }

    /// 1回の接続失敗を受けて、再試行するか諦めるかを決める。待機は残り期限を超えない。
    pub(crate) fn on_failure(&mut self, now: Millis, busy_other_session: bool, jitter_seed: u64) -> BusyRetryDecision {
        if !busy_other_session || now >= self.deadline {
            return BusyRetryDecision::GiveUp;
        }
        let after = RESUME_BACKOFF.next_delay(self.attempt, jitter_seed).min(self.deadline.saturating_sub(now));
        self.attempt = self.attempt.saturating_add(1);
        BusyRetryDecision::RetryAfter { after }
    }
}

// ── ResumePlanner ───────────────────────────────────────────────────────

/// `Backoff`/`Dial`の世代トークン(§2.2)。reducerが単調に発行し、現在待っている1つだけが有効。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ResumeToken(u64);

/// 再接続をどこへ試みるか。実体の`RelayTarget`(session secretを含む)はshellが持つ(§3-3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialPath {
    /// `--tethering-interface`のwarm standbyを昇格させる(切断ごとに最初の1回だけ)。
    WarmStandby,
    /// 最初に確立した接続先(relayならそのrelay、STUNなら元のpeer)へのbare redial。
    Primary,
    /// STUN P2Pのcross-family relay fallback(docs/adr/0006-stun-reestablish-continuity.md)。
    CrossFamily,
}

/// 1回の`Dial`の失敗の分類。`PumpFailure`等の`anyhow::Error`は載せない(shellが分類して渡す)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptFailure {
    /// `ResumeRejected(UnknownSession)`(`is_unknown_session_rejection`)。
    UnknownSession,
    /// それ以外の失敗(ネットワーク/mux/他のreject、warm standby昇格の不成立)。
    Other,
    /// RESUME自体は成功したがreplayが不整合で、この接続は捨てた。サーバがsessionを
    /// 知っていた証拠なのでUnknownSessionの連続は途切れる。
    ReplayFailed,
    /// このsessionでは再試行しても決して成功しない決定的な失敗(review 2026-09-29, PIPE-10):
    /// サーバの`OffsetGone`(clientが最後に受け取ったoffsetからはもうreplayできない)、または
    /// RESUMEが返したhelperのcommitted offsetがこのclientのC→S replayバッファの範囲外
    /// (バイトが失われ、両側のoffsetをもう突き合わせられない)。以前は`Other`/`ReplayFailed`
    /// として通常のbackoffに落ち、resume window(既定10日)が尽きるまで再試行し続け、その間
    /// `ConnectOutcome`も書かれず`isekai-ssh`の自動回復が働かなかった
    /// (`.claude/rules/always-connects.md`)。replayの*書き込み*失敗(一時的)は引き続き`ReplayFailed`。
    Unrecoverable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeEvent {
    /// data pumpがresumeすべき形で終わった(EOF-latchの判断はshell済み)。
    /// `by_network_change`: OSのネットワーク変化通知で始まった切断か。
    Disconnected { now: Millis, by_network_change: bool, jitter_seed: u64 },
    /// `Backoff{token}`の待機が終わった。`network_changed`: 新たなネットワーク変化で早期終了した。
    BackoffElapsed { token: ResumeToken, now: Millis, network_changed: bool },
    /// `Dial{token}`が成功した(RESUME/昇格とreplayまで成功)。
    AttemptOk { token: ResumeToken, now: Millis },
    /// `Dial{token}`が失敗した。
    AttemptFailed { token: ResumeToken, now: Millis, kind: AttemptFailure, jitter_seed: u64 },
    /// 待機中の表示更新の機会(TTYのライブ表示、1秒ごと)。
    Tick { now: Millis },
}

/// cross-family switchのきっかけ(ログ用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchTrigger {
    /// バックオフ待機中に新たなネットワーク変化が届いた。
    NetworkChangeWhileBackingOff,
    /// 元のSTUN peerへの失敗回数が閾値に達した、または残り期限が短くなった。
    FailuresOrDeadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GiveUpReason {
    /// `now >= deadline`(= `disconnected_at + resume_window`)。
    DeadlineExceeded { exceeded_by: Duration },
    /// UnknownSessionが閾値回連続、かつ切断から`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`経過。
    SessionGone,
    /// `AttemptFailure::Unrecoverable`(OffsetGone / replay範囲外)が1回でも返った(PIPE-10)。
    SessionUnrecoverable,
}

/// `continuity-lost` telemetryの理由(docs/adr/0006-stun-reestablish-continuity.md §3.2 task 5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuityLost {
    RelayUnreachable,
    SessionGone,
}

impl ContinuityLost {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ContinuityLost::RelayUnreachable => "relay-unreachable",
            ContinuityLost::SessionGone => "session-gone",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeCmd {
    /// 「再接続中 (elapsed/resume_window)」を表示する(猶予を過ぎたときだけ返る)。
    ShowReconnecting { elapsed: Duration, resume_window: Duration },
    /// `after`待ってから`BackoffElapsed{token}`を返す(待機中のネットワーク変化で早期終了してよい)。
    Backoff { token: ResumeToken, after: Duration },
    /// `path`へ再接続を試み、`AttemptOk{token}`/`AttemptFailed{token}`を返す。
    Dial { token: ResumeToken, path: DialPath },
    /// cross-family relayへ切り替えた(以降の`Dial`は`DialPath::CrossFamily`)。
    SwitchedToCrossFamily { trigger: SwitchTrigger },
    /// 直前の`Dial`の失敗を報告する(試行番号`attempt`付き)。give-upになった試行では返らない。
    ReportAttemptFailure { attempt: u32 },
    /// 再接続に成功した。`announce`: 「reconnected.」を表示・通知するか(猶予を過ぎていたか)。
    /// `cross_family_switched`: この切断中にcross-familyへ切り替えて成功した(telemetry用)。
    Resumed { via: DialPath, announce: bool, cross_family_switched: bool },
    /// 再接続を諦める。shellはこれを`Err`として呼び出し元へ返す(03224b11: `Ok`にしない)。
    GiveUp { reason: GiveUpReason, resume_window: Duration, notify_os: bool, continuity_lost: Option<ContinuityLost> },
}

/// [`ResumePlanner::new`]の入力(すべて接続確立時に決まる値。秘密は含まない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResumePlannerConfig {
    /// サーバが実際に認めたresume grace(秒、`0`は不明=既定値)。
    pub(crate) effective_resume_grace_secs: u32,
    /// STUN P2Pのclient側clamp(`STUN_RESUME_GIVE_UP_WINDOW`)。relayは`None`。
    pub(crate) max_resume_window: Option<Duration>,
    pub(crate) has_cross_family_target: bool,
    pub(crate) has_warm_standby: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Awaiting {
    Backoff(ResumeToken),
    Dial(ResumeToken, DialPath),
}

/// 1回の切断(episode)の状態。旧`resume_with_backoff_until_deadline`のローカル変数群。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Episode {
    disconnected_at: Millis,
    resume_window: Duration,
    deadline: Millis,
    max_resume_window: Option<Duration>,
    switched_this_call: bool,
    stun_failures: u32,
    switch_attempts_before_cross_family: u32,
    attempt: u32,
    awaiting: Awaiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connected,
    Recovering(Episode),
    GaveUp,
}

/// `run_resume_loop` 1回分(1 session)の再接続判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumePlanner {
    effective_resume_grace_secs: u32,
    has_cross_family_target: bool,
    has_warm_standby: bool,
    // ── session全体で持ち越す値(旧`run_resume_loop`のローカル変数) ──
    max_resume_window: Option<Duration>,
    resume_window: Duration,
    /// 過去のepisodeでcross-familyへの切り替えが成功した(以後ずっとそこへdialする)。
    on_cross_family: bool,
    /// `UnknownSession`が何回連続で返ってきたか(旧`ResumeLoopState::consecutive_unknown_session`)。
    /// サーバ側`RESUME`ハンドラは「session_idがテーブルに無い」(本当に消滅)だけでなく
    /// 「テーブルにはあるがまだparkされていない」(直前のdata streamのresetをまだ処理し
    /// 終えていない一時的な状態)や「fencing slotが一致しない」場合も同じ
    /// `UnknownToken`/`UnknownSession`を返す(ワイヤ上で区別できない)。1回だけで即terminal
    /// 扱いすると一時的なraceを本物の消滅と誤認して、resumeできたはずのsessionを早まって
    /// 諦める(Codexレビューで指摘)。episodeをまたいで持ち越し、成功(warm standby昇格を除く)・
    /// replay失敗・UnknownSession以外の失敗でのみリセットする。
    consecutive_unknown_session: u32,
    next_token: u64,
    phase: Phase,
}

impl ResumePlanner {
    pub(crate) fn new(config: ResumePlannerConfig) -> Self {
        Self {
            effective_resume_grace_secs: config.effective_resume_grace_secs,
            has_cross_family_target: config.has_cross_family_target,
            has_warm_standby: config.has_warm_standby,
            max_resume_window: config.max_resume_window,
            resume_window: effective_resume_window(config.effective_resume_grace_secs, config.max_resume_window),
            on_cross_family: false,
            consecutive_unknown_session: 0,
            next_token: 0,
            phase: Phase::Connected,
        }
    }

    pub(crate) fn apply(&mut self, ev: ResumeEvent) -> Vec<ResumeCmd> {
        let mut cmds = Vec::new();
        match ev {
            ResumeEvent::Disconnected { now, by_network_change, jitter_seed } => {
                if self.phase != Phase::Connected {
                    return cmds;
                }
                // 期限の時計は切断検知の瞬間から(warm standby昇格の時間も期限に数える)。
                // 移設前はここで`print_reconnect_status`も呼んでいたが、経過0では猶予内なので
                // 何も表示しなかった(到達しない分岐なのでCmdにしていない)。
                self.begin_episode(now, by_network_change, now, jitter_seed, &mut cmds);
            }
            ResumeEvent::BackoffElapsed { token, now, network_changed } => {
                let Phase::Recovering(mut ep) = self.phase else { return cmds };
                if ep.awaiting != Awaiting::Backoff(token) {
                    return cmds;
                }
                // ネットワーク変化で待機が切られたときのswitch。`stun_failures >= 1`は
                // M3/R2(元の経路へ0回でswitchする穴)を、`cross_family_probe_fits`は
                // probeが1回も入らないswitch(誤った`relay-unreachable`)を防ぐ。
                // `should_switch_to_cross_family`はここでは使わない(そのOR枝は
                // `stun_failures == 0`でも真になりうる)。
                if !ep.switched_this_call
                    && network_changed
                    && ep.stun_failures >= 1
                    && cross_family_probe_fits(ep.deadline.saturating_sub(now))
                    && self.cross_family_available()
                {
                    self.switch_to_cross_family(&mut ep, now);
                    cmds.push(ResumeCmd::SwitchedToCrossFamily { trigger: SwitchTrigger::NetworkChangeWhileBackingOff });
                }
                let token = self.fresh_token();
                let path = self.dial_path(&ep);
                ep.awaiting = Awaiting::Dial(token, path);
                cmds.push(ResumeCmd::Dial { token, path });
                self.phase = Phase::Recovering(ep);
            }
            ResumeEvent::AttemptOk { token, now } => {
                let Phase::Recovering(ep) = self.phase else { return cmds };
                let Awaiting::Dial(awaited, path) = ep.awaiting else { return cmds };
                if awaited != token {
                    return cmds;
                }
                let announce = reconnect_notify_due(ep.disconnected_at, now);
                match path {
                    DialPath::WarmStandby => {
                        // 昇格はUnknownSessionの連続もswitch状態も変えない(移設前と同じ)。
                        cmds.push(ResumeCmd::Resumed { via: path, announce, cross_family_switched: false });
                    }
                    DialPath::Primary | DialPath::CrossFamily => {
                        // RESUME_ACKはsessionが既知でparkされていた証拠。
                        self.consecutive_unknown_session = 0;
                        if ep.switched_this_call {
                            // 切り替え先が一度成功したら、以後のepisodeはそこへ、relayの
                            // grace(clampなし)でresumeする(docs/adr/0006-stun-reestablish-continuity.md §3.2 task 7)。
                            self.on_cross_family = true;
                            self.max_resume_window = None;
                            self.resume_window = effective_resume_window(self.effective_resume_grace_secs, None);
                        }
                        cmds.push(ResumeCmd::Resumed { via: path, announce, cross_family_switched: ep.switched_this_call });
                    }
                }
                self.phase = Phase::Connected;
            }
            ResumeEvent::AttemptFailed { token, now, kind, jitter_seed } => {
                let Phase::Recovering(mut ep) = self.phase else { return cmds };
                let Awaiting::Dial(awaited, path) = ep.awaiting else { return cmds };
                if awaited != token {
                    return cmds;
                }
                match (path, kind) {
                    (
                        DialPath::WarmStandby,
                        AttemptFailure::UnknownSession | AttemptFailure::Other | AttemptFailure::ReplayFailed | AttemptFailure::Unrecoverable,
                    ) => {
                        // 昇格の不成立は通常のbackoffループへそのまま落ちる(遅延の最適化で
                        // あって正しさの依存ではない)。失敗の表示はshellが昇格時に済ませている。
                        // shellは昇格の失敗を`Unrecoverable`に分類しない(通常のresumeで判定する)。
                    }
                    (DialPath::Primary | DialPath::CrossFamily, AttemptFailure::Unrecoverable) => {
                        // 決定的な失敗: 同じsessionへの再試行は同じ結果を再現するだけなので、
                        // 期限を待たずに即座に諦める(PIPE-10)。shellのGiveUpが`Err`を返し、
                        // `write_connect_outcome_for_wrapper`経由で`isekai-ssh`の自動回復が働く。
                        let continuity_lost = self.continuity_lost_if_applicable(&ep, ContinuityLost::SessionGone);
                        cmds.push(ResumeCmd::GiveUp {
                            reason: GiveUpReason::SessionUnrecoverable,
                            resume_window: ep.resume_window,
                            notify_os: ep.max_resume_window.is_none(),
                            continuity_lost,
                        });
                        self.phase = Phase::GaveUp;
                        return cmds;
                    }
                    (DialPath::Primary | DialPath::CrossFamily, AttemptFailure::ReplayFailed) => {
                        self.consecutive_unknown_session = 0;
                        cmds.push(ResumeCmd::ReportAttemptFailure { attempt: ep.attempt });
                    }
                    (DialPath::Primary | DialPath::CrossFamily, AttemptFailure::UnknownSession | AttemptFailure::Other) => {
                        if !ep.switched_this_call {
                            ep.stun_failures = ep.stun_failures.saturating_add(1);
                        }
                        // UnknownSession 1回は一時的なnot-yet-parked raceと区別できないので、
                        // 閾値回連続 *かつ* 切断から下限時間経過でだけ諦める(cc5fb926 →
                        // 71292e68 → dbb80d56)。
                        let (streak, should_give_up) = update_unknown_session_streak(
                            self.consecutive_unknown_session,
                            kind == AttemptFailure::UnknownSession,
                            now.saturating_sub(ep.disconnected_at),
                        );
                        self.consecutive_unknown_session = streak;
                        if should_give_up {
                            let continuity_lost = self.continuity_lost_if_applicable(&ep, ContinuityLost::SessionGone);
                            cmds.push(ResumeCmd::GiveUp {
                                reason: GiveUpReason::SessionGone,
                                resume_window: ep.resume_window,
                                notify_os: ep.max_resume_window.is_none(),
                                continuity_lost,
                            });
                            self.phase = Phase::GaveUp;
                            return cmds;
                        }
                        // give-upしなかった後でだけswitchを判定する(N1: 同じ試行でswitchと
                        // session-gone give-upが重なると、試してもいないcross-familyを
                        // continuity-lostの分母に数えてしまう)。
                        if !ep.switched_this_call
                            && should_switch_to_cross_family(ep.stun_failures, ep.switch_attempts_before_cross_family, ep.deadline.saturating_sub(now))
                            && self.cross_family_available()
                        {
                            self.switch_to_cross_family(&mut ep, now);
                            cmds.push(ResumeCmd::SwitchedToCrossFamily { trigger: SwitchTrigger::FailuresOrDeadline });
                        }
                        cmds.push(ResumeCmd::ReportAttemptFailure { attempt: ep.attempt });
                    }
                }
                self.loop_top(ep, now, jitter_seed, &mut cmds);
            }
            ResumeEvent::Tick { now } => {
                if let Phase::Recovering(ep) = self.phase {
                    if reconnect_notify_due(ep.disconnected_at, now) {
                        cmds.push(ResumeCmd::ShowReconnecting {
                            elapsed: now.saturating_sub(ep.disconnected_at),
                            resume_window: ep.resume_window,
                        });
                    }
                }
            }
        }
        cmds
    }

    /// `disconnected_at`から始まるepisodeを作る。windowと期限は常にsessionの現在値
    /// (`self.resume_window`、STUNのclampやcross-family成功後の非clampを反映済み)から導出する。
    /// `first_check_at`は最初のループ先頭判定の時刻(本番は常に`disconnected_at`)。
    fn begin_episode(&mut self, disconnected_at: Millis, by_network_change: bool, first_check_at: Millis, jitter_seed: u64, cmds: &mut Vec<ResumeCmd>) {
        let resume_window = self.resume_window;
        let mut ep = Episode {
            disconnected_at,
            resume_window,
            deadline: millis_after(disconnected_at, resume_window),
            max_resume_window: self.max_resume_window,
            switched_this_call: false,
            stun_failures: 0,
            // M3: ネットワーク変化で始まったepisodeでも、元の経路へ最低1回は試してからswitchする
            // (OSの変化通知は到達性に無関係なもの(VPN/tailscaleのflap等)でも発火するため)。
            switch_attempts_before_cross_family: if by_network_change { 1 } else { STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS },
            attempt: 0,
            awaiting: Awaiting::Backoff(ResumeToken(0)), // 下で必ず上書きする
        };
        if self.has_warm_standby {
            let token = self.fresh_token();
            ep.awaiting = Awaiting::Dial(token, DialPath::WarmStandby);
            cmds.push(ResumeCmd::Dial { token, path: DialPath::WarmStandby });
            self.phase = Phase::Recovering(ep);
        } else {
            self.loop_top(ep, first_check_at, jitter_seed, cmds);
        }
    }

    /// 旧ループ先頭: 期限を過ぎていれば諦め、そうでなければ次のbackoffを置く。
    /// backoffの長さは残り期限を超えない(期限を越えて再試行しない)。
    fn loop_top(&mut self, mut ep: Episode, now: Millis, jitter_seed: u64, cmds: &mut Vec<ResumeCmd>) {
        if now >= ep.deadline {
            let continuity_lost = self.continuity_lost_if_applicable(&ep, ContinuityLost::RelayUnreachable);
            cmds.push(ResumeCmd::GiveUp {
                reason: GiveUpReason::DeadlineExceeded { exceeded_by: now.saturating_sub(ep.deadline) },
                resume_window: ep.resume_window,
                notify_os: ep.max_resume_window.is_none(),
                continuity_lost,
            });
            self.phase = Phase::GaveUp;
            return;
        }
        let after = RESUME_BACKOFF.next_delay(ep.attempt, jitter_seed).min(ep.deadline.saturating_sub(now));
        ep.attempt = ep.attempt.saturating_add(1);
        let token = self.fresh_token();
        ep.awaiting = Awaiting::Backoff(token);
        cmds.push(ResumeCmd::Backoff { token, after });
        self.phase = Phase::Recovering(ep);
    }

    /// 切り替えの副作用一式(旧`apply_cross_family_switch`): 期限を切り替えの瞬間から
    /// `CROSS_FAMILY_SWITCH_DEADLINE`(下限`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`)に張り直し、
    /// backoffの試行回数を0へ戻す。UnknownSessionの連続はリセットしない(sessionについての
    /// サーバの主張で、どの経路で聞いたかに依らない、OK-2)。新しい期限もサーバのgrantを
    /// 超えない(`effective_resume_window`のclamp)。
    fn switch_to_cross_family(&self, ep: &mut Episode, now: Millis) {
        let budget = cross_family_switch_budget(ep.disconnected_at, now, self.effective_resume_grace_secs);
        ep.max_resume_window = budget.max_resume_window;
        ep.resume_window = budget.resume_window;
        ep.deadline = budget.deadline;
        ep.switched_this_call = true;
        ep.attempt = 0;
    }

    fn cross_family_available(&self) -> bool {
        self.has_cross_family_target && !self.on_cross_family
    }

    fn dial_path(&self, ep: &Episode) -> DialPath {
        if self.on_cross_family || ep.switched_this_call {
            DialPath::CrossFamily
        } else {
            DialPath::Primary
        }
    }

    /// cross-family target上でgive-upしたときだけ`continuity-lost`を記録する(旧
    /// `record_continuity_lost_if_applicable`のガード)。
    fn continuity_lost_if_applicable(&self, ep: &Episode, reason: ContinuityLost) -> Option<ContinuityLost> {
        (self.on_cross_family || ep.switched_this_call).then_some(reason)
    }

    fn fresh_token(&mut self) -> ResumeToken {
        self.next_token = self.next_token.saturating_add(1);
        ResumeToken(self.next_token)
    }

    /// テスト専用: `disconnected_at`に切断し、最初のループ先頭判定を`first_check_at`で行う
    /// (shellの配線テストが、既に期限切れのepisodeを実時間を待たずに作るため)。windowと期限は
    /// `Disconnected`と同じく**reducer自身が**sessionの値から計算する(差し替えない)。そのため
    /// STUNのclampが失われればこの入口経由のテストも落ちる(PR #164レビューM1)。
    #[cfg(test)]
    pub(crate) fn begin_episode_for_test(&mut self, disconnected_at: Millis, first_check_at: Millis, jitter_seed: u64) -> Vec<ResumeCmd> {
        let mut cmds = Vec::new();
        if self.phase == Phase::Connected {
            self.begin_episode(disconnected_at, false, first_check_at, jitter_seed, &mut cmds);
        }
        cmds
    }

    #[cfg(test)]
    fn episode(&self) -> Option<Episode> {
        match self.phase {
            Phase::Recovering(ep) => Some(ep),
            Phase::Connected | Phase::GaveUp => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GRACE_LONG: u32 = 6 * 60 * 60;

    fn relay_config() -> ResumePlannerConfig {
        ResumePlannerConfig { effective_resume_grace_secs: GRACE_LONG, max_resume_window: None, has_cross_family_target: false, has_warm_standby: false }
    }

    fn backoff_token(cmds: &[ResumeCmd]) -> ResumeToken {
        for cmd in cmds {
            if let ResumeCmd::Backoff { token, .. } = cmd {
                return *token;
            }
        }
        panic!("expected a Backoff in {cmds:?}");
    }

    fn dial(cmds: &[ResumeCmd]) -> (ResumeToken, DialPath) {
        for cmd in cmds {
            if let ResumeCmd::Dial { token, path } = cmd {
                return (*token, *path);
            }
        }
        panic!("expected a Dial in {cmds:?}");
    }

    fn give_up(cmds: &[ResumeCmd]) -> Option<GiveUpReason> {
        cmds.iter().find_map(|c| match c {
            ResumeCmd::GiveUp { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    /// Disconnected → (Backoff → Dial → 失敗)×n を、`times[i]`の時刻に失敗させて流す。
    fn fail_at(planner: &mut ResumePlanner, times: &[u64], kind: AttemptFailure) -> Vec<ResumeCmd> {
        let cmds = planner.apply(ResumeEvent::Disconnected { now: Millis(0), by_network_change: false, jitter_seed: 1 });
        keep_failing(planner, cmds, times, kind)
    }

    /// `cmds`(Backoffを含む)の続きから、`times[i]`の時刻にdialさせて失敗させる。
    fn keep_failing(planner: &mut ResumePlanner, mut cmds: Vec<ResumeCmd>, times: &[u64], kind: AttemptFailure) -> Vec<ResumeCmd> {
        for &t in times {
            let token = backoff_token(&cmds);
            let dial_cmds = planner.apply(ResumeEvent::BackoffElapsed { token, now: Millis(t), network_changed: false });
            let (token, _) = dial(&dial_cmds);
            cmds = planner.apply(ResumeEvent::AttemptFailed { token, now: Millis(t), kind, jitter_seed: 1 });
            if give_up(&cmds).is_some() {
                break;
            }
        }
        cmds
    }

    fn give_up_cmd(cmds: &[ResumeCmd]) -> Option<ResumeCmd> {
        cmds.iter().copied().find(|c| matches!(c, ResumeCmd::GiveUp { .. }))
    }

    // ── windowの値そのものの固定(PR #164レビューM1) ──────────────────────
    // 期待値はreducerの関数ではなく、移設前の定数(STUN_RESUME_GIVE_UP_WINDOW = 120秒、
    // CROSS_FAMILY_SWITCH_DEADLINE = 45秒、UNKNOWN_SESSION_MIN_ELAPSED_FLOOR = 30秒)の
    // リテラル値で書く。

    fn stun_config(has_cross_family_target: bool) -> ResumePlannerConfig {
        ResumePlannerConfig {
            effective_resume_grace_secs: GRACE_LONG,
            max_resume_window: Some(Duration::from_secs(120)),
            has_cross_family_target,
            has_warm_standby: false,
        }
    }

    /// STUNのclamp(120秒)が実際の期限になる。`ResumePlanner::new`がclampを落とすと、6時間の
    /// grantまで諦めずに120秒でGiveUpが出ないので落ちる。relayはclampされない。
    #[test]
    fn stun_clamp_is_the_real_deadline_and_relay_is_unclamped() {
        let mut p = ResumePlanner::new(stun_config(false));
        assert_eq!(give_up(&fail_at(&mut p, &[119_999], AttemptFailure::Other)), None, "STUN must keep retrying just before 120s");
        let mut p = ResumePlanner::new(stun_config(false));
        assert_eq!(
            give_up_cmd(&fail_at(&mut p, &[120_000], AttemptFailure::Other)),
            Some(ResumeCmd::GiveUp {
                reason: GiveUpReason::DeadlineExceeded { exceeded_by: Duration::ZERO },
                resume_window: Duration::from_secs(120),
                notify_os: false,
                continuity_lost: None,
            }),
            "STUN must give up at exactly 120s, without a desktop notification"
        );
        let mut p = ResumePlanner::new(relay_config());
        assert_eq!(give_up(&fail_at(&mut p, &[120_000, 3_600_000], AttemptFailure::Other)), None, "relay uses the unclamped grant");
    }

    /// C1/R1: 元のSTUN peerへの5回目の失敗(t=15.5s)でswitchし、その瞬間から45秒
    /// (= 切断から60.5秒)を新しい期限にする。C1(switch時に`max_resume_window = None`)が戻ると
    /// 6時間のgrantまで諦めないので60.5sでのGiveUpが無くなり、R1(切断時刻から45秒)が戻ると
    /// 60.499sで既に諦めるので、どちらでも落ちる。
    #[test]
    fn cross_family_switch_budget_is_45s_from_the_switch_itself() {
        let mut p = ResumePlanner::new(stun_config(true));
        let cmds = fail_at(&mut p, &[500, 1_500, 3_500, 7_500, 15_500], AttemptFailure::Other);
        assert!(
            cmds.contains(&ResumeCmd::SwitchedToCrossFamily { trigger: SwitchTrigger::FailuresOrDeadline }),
            "5 failures against the STUN peer must switch: {cmds:?}"
        );
        assert_eq!(p.episode().map(|e| e.resume_window), Some(Duration::from_millis(60_500)));
        let mut before_deadline = p.clone();
        assert_eq!(give_up(&keep_failing(&mut before_deadline, cmds.clone(), &[60_499], AttemptFailure::Other)), None);
        assert_eq!(
            give_up_cmd(&keep_failing(&mut p, cmds, &[60_500], AttemptFailure::Other)),
            Some(ResumeCmd::GiveUp {
                reason: GiveUpReason::DeadlineExceeded { exceeded_by: Duration::ZERO },
                resume_window: Duration::from_millis(60_500),
                notify_os: false,
                continuity_lost: Some(ContinuityLost::RelayUnreachable),
            })
        );
    }

    /// task 7: cross-familyで一度成功したら、以後のepisodeはそこへ、clampなしのgrantでresumeする
    /// (成功時の`self.resume_window`更新が消えると、次のepisodeが120秒で諦めて落ちる)。
    #[test]
    fn after_a_cross_family_success_later_episodes_use_the_unclamped_grant() {
        let mut p = ResumePlanner::new(stun_config(true));
        let cmds = fail_at(&mut p, &[500, 1_500, 3_500, 7_500, 15_500], AttemptFailure::Other);
        let token = backoff_token(&cmds);
        let (token, path) = dial(&p.apply(ResumeEvent::BackoffElapsed { token, now: Millis(16_000), network_changed: false }));
        assert_eq!(path, DialPath::CrossFamily);
        let ok = p.apply(ResumeEvent::AttemptOk { token, now: Millis(16_000) });
        assert!(ok.contains(&ResumeCmd::Resumed { via: DialPath::CrossFamily, announce: true, cross_family_switched: true }), "{ok:?}");

        let cmds = p.apply(ResumeEvent::Disconnected { now: Millis(100_000), by_network_change: false, jitter_seed: 1 });
        assert_eq!(p.episode().map(|e| e.resume_window), Some(Duration::from_secs(u64::from(GRACE_LONG))));
        let cmds = keep_failing(&mut p, cmds, &[100_000 + 120_000, 100_000 + 3_600_000], AttemptFailure::Other);
        assert_eq!(give_up(&cmds), None, "the second episode must not be clamped to 120s");
        assert!(!cmds.iter().any(|c| matches!(c, ResumeCmd::SwitchedToCrossFamily { .. })), "never switch again");
    }

    // ── 過去に3回直し直された方針の表テスト(境界値) ───────────────────

    /// cc5fb926(1回で諦めた)→ 71292e68(3回連続で諦めたがroaming中t≈3.5sで誤爆)→
    /// dbb80d56(+30秒の下限)。3つの版それぞれの誤りが再発しないことを固定する。
    #[test]
    fn unknown_session_give_up_history_is_pinned() {
        // cc5fb926の誤り: 1回のUnknownSessionで諦める。
        let mut p = ResumePlanner::new(relay_config());
        assert_eq!(give_up(&fail_at(&mut p, &[60_000], AttemptFailure::UnknownSession)), None);
        // 71292e68の誤り: 3回連続なら経過時間に関係なく諦める(t≈0.5s,1.5s,3.5s)。
        let mut p = ResumePlanner::new(relay_config());
        assert_eq!(give_up(&fail_at(&mut p, &[500, 1_500, 3_500, 7_500, 15_500, 25_500], AttemptFailure::UnknownSession)), None);
        // dbb80d56: 3回連続 *かつ* 切断から30秒(ちょうど)で諦める。
        let mut p = ResumePlanner::new(relay_config());
        assert_eq!(give_up(&fail_at(&mut p, &[500, 1_500, 30_000], AttemptFailure::UnknownSession)), Some(GiveUpReason::SessionGone));
        // 30秒の1ms手前では諦めない。
        let mut p = ResumePlanner::new(relay_config());
        assert_eq!(give_up(&fail_at(&mut p, &[500, 1_500, 29_999], AttemptFailure::UnknownSession)), None);
    }

    /// PIPE-10: `OffsetGone`/replay範囲外(`Unrecoverable`)は1回目でその場で諦める(10日のwindowを
    /// 待たない)。それ以前の一時的な失敗が何回あっても同じで、`ReportAttemptFailure`は出さない。
    #[test]
    fn unrecoverable_failure_gives_up_immediately() {
        let mut p = ResumePlanner::new(relay_config());
        let cmds = fail_at(&mut p, &[500], AttemptFailure::Unrecoverable);
        assert_eq!(
            give_up_cmd(&cmds),
            Some(ResumeCmd::GiveUp {
                reason: GiveUpReason::SessionUnrecoverable,
                resume_window: Duration::from_secs(u64::from(GRACE_LONG)),
                notify_os: true,
                continuity_lost: None,
            })
        );
        assert!(!cmds.iter().any(|c| matches!(c, ResumeCmd::ReportAttemptFailure { .. })), "{cmds:?}");
        assert_eq!(p.phase, Phase::GaveUp);

        let mut p = ResumePlanner::new(relay_config());
        let cmds = fail_at(&mut p, &[500, 1_500], AttemptFailure::Other);
        assert_eq!(give_up(&cmds), None);
        assert_eq!(give_up(&keep_failing(&mut p, cmds, &[3_500], AttemptFailure::Unrecoverable)), Some(GiveUpReason::SessionUnrecoverable));
    }

    /// 03224b11: 期限超過は`GiveUp`(shellが`Err`にする)であって、黙って接続済みへ戻らない。
    #[test]
    fn deadline_exceeded_gives_up_instead_of_silently_returning_to_connected() {
        let mut p = ResumePlanner::new(ResumePlannerConfig { effective_resume_grace_secs: 10, ..relay_config() });
        let cmds = fail_at(&mut p, &[500, 1_500, 3_500, 7_500, 10_000], AttemptFailure::Other);
        assert_eq!(give_up(&cmds), Some(GiveUpReason::DeadlineExceeded { exceeded_by: Duration::ZERO }));
        assert_eq!(p.phase, Phase::GaveUp);
    }

    /// 3d5e0da5: BUSY_OTHER_SESSIONの期限はresume graceと無関係な固定180秒。
    #[test]
    fn busy_other_session_window_is_a_fixed_180s_from_the_first_attempt() {
        assert_eq!(BUSY_OTHER_SESSION_RETRY_WINDOW, Duration::from_secs(180));
        let mut r = BusyOtherSessionRetry::start(Millis(1_000), BUSY_OTHER_SESSION_RETRY_WINDOW);
        assert!(matches!(r.on_failure(Millis(180_999), true, 0), BusyRetryDecision::RetryAfter { after, .. } if after <= Duration::from_millis(1)));
        assert_eq!(r.on_failure(Millis(181_000), true, 0), BusyRetryDecision::GiveUp);
        let mut r = BusyOtherSessionRetry::start(Millis(0), BUSY_OTHER_SESSION_RETRY_WINDOW);
        assert_eq!(r.on_failure(Millis(0), false, 0), BusyRetryDecision::GiveUp, "BUSY以外は再試行しない");
    }

    #[test]
    fn reconnect_notice_respects_the_15s_grace() {
        assert!(!reconnect_notify_due(Millis(1_000), Millis(15_999)));
        assert!(reconnect_notify_due(Millis(1_000), Millis(16_000)));
        assert!(!reconnect_notify_due(Millis(20_000), Millis(1_000)), "時刻の逆行は未経過扱い");
    }

    // ── proptest: 任意のEvent列 ─────────────────────────────────────────

    #[derive(Debug, Clone)]
    enum Step {
        /// 時刻を進める/戻す(非単調な`now`、§2.2必須プロパティ)。
        Advance(i64),
        Disconnect { by_network_change: bool },
        /// 現在待っているBackoff/Dialへ応答する。
        Respond { outcome: u8, network_changed: bool },
        /// 古い/未来のtokenで応答する。
        Stale { token_delta: u64, outcome: u8 },
        Tick,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            3 => (-20_000i64..200_000).prop_map(Step::Advance),
            1 => any::<bool>().prop_map(|by_network_change| Step::Disconnect { by_network_change }),
            6 => (prop_oneof![9 => 0u8..4, 1 => Just(4u8)], any::<bool>()).prop_map(|(outcome, network_changed)| Step::Respond { outcome, network_changed }),
            1 => (1u64..4, 0u8..4).prop_map(|(token_delta, outcome)| Step::Stale { token_delta, outcome }),
            1 => Just(Step::Tick),
        ]
    }

    fn config() -> impl Strategy<Value = ResumePlannerConfig> {
        (
            prop_oneof![Just(0u32), 1u32..300, Just(GRACE_LONG)],
            prop_oneof![Just(None), (20u64..200).prop_map(|s| Some(Duration::from_secs(s)))],
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(effective_resume_grace_secs, max_resume_window, has_cross_family_target, has_warm_standby)| ResumePlannerConfig {
                effective_resume_grace_secs,
                max_resume_window,
                has_cross_family_target,
                has_warm_standby,
            })
    }

    fn outcome_event(outcome: u8, token: ResumeToken, now: Millis, seed: u64) -> ResumeEvent {
        match outcome {
            0 => ResumeEvent::AttemptOk { token, now },
            1 => ResumeEvent::AttemptFailed { token, now, kind: AttemptFailure::UnknownSession, jitter_seed: seed },
            2 => ResumeEvent::AttemptFailed { token, now, kind: AttemptFailure::Other, jitter_seed: seed },
            3 => ResumeEvent::AttemptFailed { token, now, kind: AttemptFailure::ReplayFailed, jitter_seed: seed },
            _ => ResumeEvent::AttemptFailed { token, now, kind: AttemptFailure::Unrecoverable, jitter_seed: seed },
        }
    }

    /// テスト側の独立モデル: UnknownSessionの連続回数(planner内部を見ずに数える)。
    #[derive(Default)]
    struct Model {
        disconnected_at: Option<Millis>,
        unknown_streak: u32,
        stun_failures: u32,
        switched: bool,
        ever_switched: bool,
        /// cross-familyへの切り替えが成功した(以後のepisodeはclampなしのgrant、task 7)。
        cf_succeeded: bool,
    }

    /// 移設前の`run_resume_loop`が各episodeに使ったwindowを、**移設前の定数と式から独立に**
    /// 計算する(reducerの`effective_resume_window`を呼ばない。PR #164レビューM1): grace 0は
    /// `crate::DEFAULT_RESUME_WINDOW`、STUN(`max`あり)はそれでclamp、cross-family成功後はclampなし。
    fn expected_episode_window(cfg: &ResumePlannerConfig, cf_succeeded: bool) -> Duration {
        let grant = if cfg.effective_resume_grace_secs == 0 {
            crate::DEFAULT_RESUME_WINDOW
        } else {
            Duration::from_secs(u64::from(cfg.effective_resume_grace_secs))
        };
        match (cf_succeeded, cfg.max_resume_window) {
            (false, Some(max)) => grant.min(max),
            (true, _) | (false, None) => grant,
        }
    }

    /// 移設前の`cross_family_switch_budget`の式(C1: 切り替え時に必ず有界にする、R1: 切り替えの
    /// 瞬間から45秒、下限30秒、grantでclamp)を、移設前のリテラル値で独立に計算する。
    fn expected_switch_window(cfg: &ResumePlannerConfig, elapsed_since_disconnect: Duration) -> Duration {
        let grant = expected_episode_window(cfg, true);
        grant.min((elapsed_since_disconnect + Duration::from_secs(45)).max(Duration::from_secs(30)))
    }

    proptest! {
        /// give-up/期限/backoff/switch/stale-tokenの不変条件を、任意のEvent列と非単調な`now`で検査する。
        #[test]
        fn resume_planner_invariants(cfg in config(), steps in prop::collection::vec(step(), 1..80), seed in any::<u64>()) {
            let mut p = ResumePlanner::new(cfg);
            let mut model = Model::default();
            let mut now: u64 = 1_000_000;
            let grant = resume_window_for(cfg.effective_resume_grace_secs);
            for st in steps {
                let before = p.clone();
                let ep_before = p.episode();
                let mut stale = false;
                let ev = match st {
                    Step::Advance(d) => { now = now.saturating_add_signed(d); continue; }
                    Step::Disconnect { by_network_change } => ResumeEvent::Disconnected { now: Millis(now), by_network_change, jitter_seed: seed },
                    Step::Tick => ResumeEvent::Tick { now: Millis(now) },
                    Step::Respond { outcome, network_changed } => match ep_before.map(|e| e.awaiting) {
                        Some(Awaiting::Backoff(token)) => ResumeEvent::BackoffElapsed { token, now: Millis(now), network_changed },
                        Some(Awaiting::Dial(token, _)) => outcome_event(outcome, token, Millis(now), seed),
                        None => ResumeEvent::Tick { now: Millis(now) },
                    },
                    Step::Stale { token_delta, outcome } => {
                        let Some(e) = ep_before else { continue };
                        let current = match e.awaiting { Awaiting::Backoff(t) | Awaiting::Dial(t, _) => t.0 };
                        let token = ResumeToken(if token_delta % 2 == 0 { current.saturating_add(token_delta) } else { current.saturating_sub(token_delta) });
                        if token.0 == current { continue; }
                        stale = true;
                        if outcome == 3 { ResumeEvent::BackoffElapsed { token, now: Millis(now), network_changed: true } } else { outcome_event(outcome, token, Millis(now), seed) }
                    }
                };
                let cmds = p.apply(ev);

                // (S) stale tokenのEventはStateを変えずCmdも返さない(§2.2必須プロパティ)。
                if stale {
                    prop_assert!(cmds.is_empty(), "stale event produced {cmds:?}");
                    prop_assert_eq!(&p, &before);
                    continue;
                }

                // 独立モデルの更新(Event側から)。
                if let ResumeEvent::Disconnected { now: t, .. } = ev {
                    if before.phase == Phase::Connected {
                        model.disconnected_at = Some(t);
                        model.stun_failures = 0;
                        model.switched = false;
                        // (V1) episodeのwindowと期限は、移設前の式(STUN clamp / 成功後は非clamp)の値そのもの。
                        let expected = expected_episode_window(&cfg, model.cf_succeeded);
                        let ep = p.episode().expect("a disconnect from Connected must start an episode");
                        prop_assert_eq!(ep.resume_window, expected, "episode window != pre-Step-5 formula");
                        prop_assert_eq!(ep.disconnected_at, t, "episode not anchored at the disconnect");
                        prop_assert_eq!(ep.deadline, millis_after(t, expected), "deadline != disconnect + window");
                    }
                }
                let mut unknown_failure_now = false;
                let mut unrecoverable_now = false;
                if let (ResumeEvent::AttemptFailed { kind, .. }, Some(Awaiting::Dial(_, path))) = (ev, ep_before.map(|e| e.awaiting)) {
                    if path != DialPath::WarmStandby {
                        match kind {
                            AttemptFailure::UnknownSession => { model.unknown_streak += 1; unknown_failure_now = true; }
                            AttemptFailure::Other | AttemptFailure::ReplayFailed => model.unknown_streak = 0,
                            AttemptFailure::Unrecoverable => unrecoverable_now = true,
                        }
                        if matches!(kind, AttemptFailure::UnknownSession | AttemptFailure::Other) && !model.switched {
                            model.stun_failures += 1;
                        }
                    }
                }
                if let (ResumeEvent::AttemptOk { .. }, Some(Awaiting::Dial(_, path))) = (ev, ep_before.map(|e| e.awaiting)) {
                    if path != DialPath::WarmStandby { model.unknown_streak = 0; }
                }

                let elapsed = model.disconnected_at.map(|d| Millis(now).saturating_sub(d)).unwrap_or_default();
                let ever_switched_before = model.ever_switched;
                let mut switched_now = false;
                let mut gave_up = false;
                for cmd in &cmds {
                    match *cmd {
                        ResumeCmd::GiveUp { reason, resume_window, .. } => {
                            gave_up = true;
                            // (G0) 期限はサーバのgrantを超えない(clientがparkの失効後まで再試行しない)。
                            prop_assert!(resume_window <= grant, "resume window {resume_window:?} > server grant {grant:?}");
                            match reason {
                                GiveUpReason::SessionGone => {
                                    // (G1) UnknownSessionのgive-upは「今回もUnknownSession」「閾値回連続」
                                    //      「切断から下限時間経過」の3つがすべて揃ったときだけ。
                                    prop_assert!(unknown_failure_now, "SessionGone give-up on an attempt that was not an UnknownSession rejection");
                                    prop_assert!(model.unknown_streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD, "gave up at streak {}", model.unknown_streak);
                                    prop_assert!(elapsed >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR, "gave up only {elapsed:?} after disconnect");
                                }
                                GiveUpReason::SessionUnrecoverable => {
                                    // (G3) 決定的失敗のgive-upは、今回のPrimary/CrossFamilyへの試行が
                                    //      `Unrecoverable`だったときだけ(PIPE-10)。
                                    prop_assert!(unrecoverable_now, "SessionUnrecoverable give-up on an attempt that was not Unrecoverable");
                                }
                                GiveUpReason::DeadlineExceeded { exceeded_by } => {
                                    // (G2) 期限のgive-upは now >= disconnected_at + resume_window のときだけ
                                    //      (時刻が逆行しても誤発火しない)。
                                    prop_assert_eq!(elapsed, resume_window + exceeded_by);
                                    prop_assert!(Millis(now) >= millis_after(model.disconnected_at.unwrap(), resume_window), "deadline give-up before disconnected_at + resume_window");
                                }
                            }
                            prop_assert_eq!(p.phase, Phase::GaveUp);
                        }
                        ResumeCmd::Backoff { after, .. } => {
                            let ep = p.episode().expect("Backoff outside an episode");
                            // (B1) backoffは期限を越えない、かつ期限到達後にはbackoffしない(=諦める)。
                            prop_assert!(Millis(now) < ep.deadline, "backed off at/after the deadline instead of giving up");
                            prop_assert!(after <= ep.deadline.saturating_sub(Millis(now)), "backoff {after:?} overshoots the deadline");
                            prop_assert!(after <= RESUME_BACKOFF.max, "backoff {after:?} exceeds RESUME_BACKOFF.max");
                            prop_assert!(ep.resume_window <= grant, "episode window exceeds the server grant");
                            prop_assert_eq!(ep.deadline, millis_after(ep.disconnected_at, ep.resume_window));
                        }
                        ResumeCmd::SwitchedToCrossFamily { trigger } => {
                            // (W) switchは cross-familyがあり、未switchで、元の経路へ1回以上失敗し、
                            //     切り替え時点でprobeが1回入る残り期限があるときだけ、episodeに1回。
                            prop_assert!(cfg.has_cross_family_target && !model.ever_switched, "switched without a cross-family target, or switched twice");
                            prop_assert!(model.stun_failures >= 1, "switched with zero failed attempts against the original target (M3/R2)");
                            let ep0 = ep_before.expect("switch outside an episode");
                            prop_assert!(cross_family_probe_fits(ep0.deadline.saturating_sub(Millis(now))), "switched when not even one cross-family probe fits before the deadline");
                            if trigger == SwitchTrigger::FailuresOrDeadline {
                                prop_assert!(should_switch_to_cross_family(model.stun_failures, ep0.switch_attempts_before_cross_family, ep0.deadline.saturating_sub(Millis(now))), "FailuresOrDeadline switch while should_switch_to_cross_family is false");
                            }
                            // (V2) 切り替え後のwindowは移設前の`cross_family_switch_budget`の値そのもの
                            //      (C1: Noneにしない、R1: 切り替え時刻からの45秒)。即give-upした場合は
                            //      GiveUpが報告するwindowで見る。
                            let expected = expected_switch_window(&cfg, Millis(now).saturating_sub(ep0.disconnected_at));
                            let switched_window = match p.episode() {
                                Some(ep) => {
                                    prop_assert_eq!(ep.disconnected_at, ep0.disconnected_at, "switch must not move the disconnect anchor");
                                    prop_assert_eq!(ep.deadline, millis_after(ep0.disconnected_at, expected), "post-switch deadline != disconnect + budget");
                                    Some(ep.resume_window)
                                }
                                None => cmds.iter().find_map(|c| match c {
                                    ResumeCmd::GiveUp { resume_window, .. } => Some(*resume_window),
                                    _ => None,
                                }),
                            };
                            prop_assert_eq!(switched_window, Some(expected), "post-switch window != pre-Step-5 switch budget");
                            model.switched = true;
                            model.ever_switched = true;
                            switched_now = true;
                        }
                        ResumeCmd::ShowReconnecting { elapsed: shown, resume_window } => {
                            // (N) 再接続表示は切断から15秒の猶予を過ぎてから。
                            prop_assert!(shown >= RECONNECT_NOTIFY_GRACE, "reconnect notice inside the 15s grace");
                            prop_assert!(resume_window <= grant, "displayed window exceeds the server grant");
                        }
                        ResumeCmd::Resumed { announce, cross_family_switched, .. } => {
                            prop_assert_eq!(announce, elapsed >= RECONNECT_NOTIFY_GRACE);
                            prop_assert_eq!(cross_family_switched, model.switched);
                            if cross_family_switched {
                                model.cf_succeeded = true;
                            }
                            prop_assert_eq!(p.phase, Phase::Connected);
                        }
                        ResumeCmd::Dial { path, .. } => {
                            prop_assert!(path != DialPath::CrossFamily || model.ever_switched, "dialed CrossFamily before any switch");
                        }
                        ResumeCmd::ReportAttemptFailure { .. } => {}
                    }
                }

                // (G3') liveness: Primary/CrossFamilyへの`Unrecoverable`では必ずその場で諦める(PIPE-10)。
                if unrecoverable_now {
                    prop_assert!(gave_up, "an Unrecoverable attempt must give up immediately");
                }
                // (G1') liveness: 3条件が揃った失敗では必ず諦める(諦めすぎ・諦めなさすぎの両方向)。
                if unknown_failure_now && model.unknown_streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD && elapsed >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR {
                    prop_assert!(gave_up, "streak {} / {elapsed:?} must give up", model.unknown_streak);
                }
                // (W') switch liveness(PR #164レビューM3): 条件が揃ったら必ず切り替える
                //      (ADR_STUN_REESTABLISH_CONTINUITY §3.2 task 1「windowを待ち切る劣化」の防止)。
                if cfg.has_cross_family_target && !ever_switched_before && !gave_up {
                    if let Some(ep0) = ep_before {
                        let remaining = ep0.deadline.saturating_sub(Millis(now));
                        let failure_trigger = match (ev, ep0.awaiting) {
                            (ResumeEvent::AttemptFailed { kind: AttemptFailure::UnknownSession | AttemptFailure::Other, .. }, Awaiting::Dial(_, DialPath::Primary)) => {
                                should_switch_to_cross_family(model.stun_failures, ep0.switch_attempts_before_cross_family, remaining)
                            }
                            _ => false,
                        };
                        let network_trigger = match ev {
                            ResumeEvent::BackoffElapsed { network_changed: true, .. } => model.stun_failures >= 1 && cross_family_probe_fits(remaining),
                            _ => false,
                        };
                        if failure_trigger || network_trigger {
                            prop_assert!(switched_now, "switch conditions held (failures={}, remaining={remaining:?}) but no switch", model.stun_failures);
                        }
                    }
                }
                // (C) 接続済みへ戻るのは`Resumed`を返したときだけ(03224b11)。
                if before.phase != Phase::Connected && p.phase == Phase::Connected {
                    prop_assert!(cmds.iter().any(|c| matches!(c, ResumeCmd::Resumed { .. })), "returned to Connected without a Resumed command (03224b11)");
                }
                if gave_up {
                    model.disconnected_at = None;
                }
            }
        }

        /// BUSY_OTHER_SESSIONの再試行: 期限は開始+windowだけで決まり、BUSY以外は即座に返し、
        /// 待機は残り期限を超えず、期限到達後は必ず諦める(非単調な`now`でも)。
        #[test]
        fn busy_other_session_retry_invariants(
            start in 0u64..1_000_000,
            window_ms in 0u64..400_000,
            steps in prop::collection::vec((-50_000i64..120_000, prop::bool::weighted(0.9), any::<u64>()), 1..40),
        ) {
            let window = Duration::from_millis(window_ms);
            let deadline = start + window_ms;
            let mut r = BusyOtherSessionRetry::start(Millis(start), window);
            let mut now = start;
            for (delta, busy, seed) in steps {
                now = now.saturating_add_signed(delta);
                match r.on_failure(Millis(now), busy, seed) {
                    BusyRetryDecision::GiveUp => {
                        prop_assert!(!busy || now >= deadline, "BUSY retry gave up while busy and before the deadline");
                    }
                    BusyRetryDecision::RetryAfter { after, .. } => {
                        prop_assert!(busy && now < deadline, "BUSY retry retried a non-BUSY failure or past the deadline");
                        prop_assert!(Duration::from_millis(now) + after <= Duration::from_millis(deadline), "BUSY retry wait overshoots the deadline");
                        prop_assert!(after <= RESUME_BACKOFF.max, "BUSY retry wait exceeds RESUME_BACKOFF.max");
                    }
                }
            }
        }
    }
}
