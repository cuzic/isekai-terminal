# ADR_IOS_PARITY_IMPLEMENTATION.md — Round-2 Review

- **Reviewer**: reviewer agent (Opus), 2026-08-21
- **Target**: round-2 revision of `/home/cuzic/isekai-terminal/ADR_IOS_PARITY_IMPLEMENTATION.md`
- **Previous round**: `ADR_REVIEW_ROUND1.md` (4 blocking / 10 significant / 3 minor / 5 premortem)
- **Scope**: verification of the 4 blocking fixes, adjudication of the architect's one pushback,
  spot-check of newly added designs, answers to Q9–Q11, fresh premortem.

---

## Verdict

**Converged on all four blocking items.** B1, B2, B3, B4 are genuinely resolved — not
papered over. The architect also caught a factual error of mine (S2) and was right to
push back.

The four new findings below (**N1–N4**) are all *specification gaps in the newly added
designs*, not disagreements with any decision. None of them invalidates a choice; each is a
paragraph the ADR should contain before an implementer starts. I am explicitly **not**
calling them blocking. If the architect folds N1–N4 in, this ADR is done and should ship.

---

## 0. Concession: S2 was wrong, and the reframe was half-fair

**The architect is right.** Verified directly:

- `.github/workflows/ios-rust-core-check.yml:142-144`:
  ```
  xcodebuild test \
    -scheme IsekaiTerminalCore-Package \
    -destination 'platform=iOS Simulator,name=...'
  ```
- `ios/Package.swift:81-84` declares `.testTarget(name: "IsekaiTerminalCoreTests", ...)`
  inside `appleOnlyTargets`, which is included in `targets:` at line 89-91.

`IsekaiTerminalCore-Package` is SwiftPM's auto-generated aggregate scheme and runs every
declared test target. `TerminalFrameRendererTests` **does** execute in CI. My round-1 S2
checked `IsekaiTerminalApp.xcscheme` and the `-only-testing:` filter in
`ios-ssh-vertical-slice-check.yml` and never opened `ios-rust-core-check.yml`. The claim
"the gate never fires" and the derived "R1's mitigation is fictional" were both false.
§1.4's new CI/test-target table is a good permanent fix — it removes the whole class of
this mistake for future readers.

**On the reframe** — the ADR says S2 "実質的に指していた懸念" was wall-clock flakiness on a
shared macOS runner. In fairness to the architect and to the record: **that is not what I
meant.** I made a factual error about CI wiring; I said nothing about flakiness. The
reframe is generous rather than accurate.

But the substitution it produced is *better than what I asked for*, and I endorse it
unreservedly: asserting "the Core Text resolver is called no more than once per unique
codepoint" via an injected fake is a deterministic property that actually tests the thing
R1 is about (caching), runs on the cheap Linux job, and is immune to runner load — which
this repo has repeatedly been bitten by. Keeping a non-failing measurement in
`TerminalFrameRendererTests` for eyeball regression is the right complement. **Adopt as
written.** Recommend only that §3.6.1 be edited to say "the reviewer's factual claim was
wrong; separately, we chose a deterministic gate over a wall-clock one because of this
repo's load-flakiness history" — two independent statements — rather than attributing the
second to the review.

---

## 1. B1 (notify path A/B split) — **resolved**, with one new gap

**Resolved.** §3.5.0's `tabAlertCopy(kind:profileLabel:message:)` matches
`TabAlertNotifier.kt:116-118` exactly, including the empty-string-falls-back-to-default
behavior. Making the `message`-present branch an explicit 🟢 acceptance condition, and the
🔴 manual check "`ctl notify --title X` must arrive as `プロファイル名: X`, not the canned
text", is precisely the right regression line — R4 in the risk table names the failure
mode ("沈黙より悪い") correctly. The path A/B split, the `NotifyGenerationTracker`
placement in Logic, and the trust-boundary note are all sound.

### N1 — significant — `NotifyGenerationTracker` reset semantics are unspecified, and copying the bell pattern inherits a latent gap

§3.5b lists "世代が巻き戻った場合（セッション再生成）" as a **test case** but never states
the required **behavior**, and specifies no reset wiring. The existing precedent in this
exact file is unusually well-documented, and it says two things the ADR needs to inherit:

`TerminalSessionController.swift:75-83` (doc on `lastFiredBellGeneration`):

> `reconnect()`すると、Rust側で新しいTerminalが作られ`bell_generation`が0から再スタート
> するため、リセットしないと「新セッションのgen 1 < 旧セッションで記憶した値」で最初の
> BELを取りこぼす。`connect()`側でTask経由の非同期リセットにすると、新セッション最初の
> `onScreenUpdate`コールバックの方が先にMainActorキューで処理されうる競合を生むため…
> （Codexレビュー指摘）

**Consequence if unaddressed**: implement the tracker as a naive "fire if
`generation > last`" and, after any new logical session, every AI notification is silently
dropped until the counter climbs back past the remembered value. That is failure mode P1
again — arriving through a different door, and *harder* to diagnose than the original
because it is intermittent (works on a fresh session, dies after a reconnect).

Two specifics to write into §3.5b:

1. **Reset must be synchronous, in the same `@MainActor` context** as
   `uiState.latestScreenUpdate = nil` / `lastFiredBellGeneration = 0`
   (`TerminalSessionController.swift:559-569`). An async reset loses a documented race
   against the new session's first `onScreenUpdate`.
2. **Reset on entry to `Connected`, not only in manual `reconnect()`.** The bell reset today
   lives only in the manual `reconnect()` path (case `.disconnected, .failed` at line 558).
   Rust also creates new Terminals on paths Swift never routes through `reconnect()` —
   `spawn_reconnect_loop`, and `notify_will_enter_foreground`'s auto-reconnect
   (`orchestrator.rs:1291-1300`). If the bell path has this gap today it is a pre-existing
   latent bug; either way **#10 turns auto-reconnect from the rare path into the common
   one** (foreground resume, cold-start restore), so the tracker must not inherit it.
   Resetting on the Connected transition covers manual and automatic uniformly.

Belt-and-braces: also treat a *decrease* in `notifyGeneration` as a reset (defensive; costs
one comparison). Keep it as stated behavior, not just a test case.

---

## 2. B2 (D-1 rewording) — **resolved**

The new D-1 is precise in the way the old one was not: two explicit columns, concrete
examples in each, the `TabAlertNotifier.kt:26-31` citation as precedent, and — the part that
makes it stick — the "なぜこの区別が実害に直結するか" paragraph explaining that path B's
profile flag never reaches Rust. A future reviewer reading this will not make my mistake.

Checked for the opposite failure (does the looser wording let real suppression sneak back
in?): **no.** The Rust-only column enumerates the categories tightly — session-state-derived
suppression, policy thresholds, mirror enums, and now tmux ownership — and §3.5a
independently reasserts that `willPresent` shows every path-A notification Rust delivers.
The Swift-side allowance is bounded to exactly two named gates.

- **m1 (minor)**: one sentence would close the last crack — state that the Swift-side gates
  are *exactly* those two, and that adding a third condition to `willPresent` or to
  `TabAlertNotifier` requires amending this ADR. Without it, "OS 権限の確認" is the kind of
  category that quietly grows a "…and don't show it while the app is frontmost" clause.

---

## 3. B3 (`on_foreground_resume`) — **resolved**, with one new gap

The retraction is handled well: §1.2's ★#9 row, §3.9.3(c), and D-6 all state the same thing
consistently, and the rejected 2-way alternative is rejected for the right reason (it
collapses back into `ConnectionState` inference, i.e. the mirror state machine D-1
forbids). One callback carrying one bool, fired from both branches of an existing method,
is genuinely minimal.

### N2 — significant — three under-specified semantics on the new callback

**(a) Firing when there was nothing to maintain.** `notify_will_enter_foreground` is invoked
unconditionally for every tab by the fanout in
`TerminalTabsHostView.handleWillEnterForeground` (`:113-118`). A tab that was never
connected — or was already disconnected — sits in `BackgroundState::Foreground`, which
`orchestrator.rs:187-189` documents as "バックグラウンド遷移がそもそも意味を持たない
(未接続・既に切断済み等)". If the callback fires there with `did_reconnect: false`, the
banner renders 「復帰しました（接続は維持されています）」 over a **disconnected** terminal.
That is exactly the class of user-visible lie §3.9 exists to prevent.
→ Fire only when `background_state != Foreground` on entry, or carry a third state.
State the choice in §3.9.3(c).

**(b) `did_reconnect: true` cannot honestly mean "reconnected" at fire time.** The reconnect
branch calls `(self.shared.reconnect_attempt)(...)`, which can fail **synchronously**
(`orchestrator.rs:1305-1316` handles exactly that: it resets `phase` to `Idle` and emits
`Disconnected`). So whichever order the callback fires in, `true` means "a reconnect was
initiated", not "succeeded". The banner copy 「再接続しました」 (past tense, success) will
sometimes be immediately contradicted by a `Disconnected` callback.
→ Define the flag as *initiated*, and change the copy to 「再接続しています」 (with the
existing connection-state UI carrying the outcome). Cheap, and it keeps §3.9's honesty
property intact.

**(c) N tabs → N callbacks.** The fanout means one `on_foreground_resume` per tab per
foreground event. Per-tab banners are fine; the cold-start 「3タブを復元しました」 banner is
app-level. §3.9.3(c) should say which banners are per-tab and which are app-level, and that
the app-level one is emitted once by `TabRestoreCoordinator` rather than N times by the
callback fanout.

---

## 4. B4 (Rust tmux claim) — **resolved**, with one new gap

Placing the claim in Rust is the right call and F in §3.10.5 rejects the Swift variant for
the right reason ("これは 2026-07-27 の事故と同じ配置である"). The owner-matched release is
a genuine improvement over Android's owner-less `Set`, and the architect's simplification
claim is correct: with `owner_id = tabId`, exactly one tab holds the claim, so Android's
"release only when the last tab for this profile closes" refcounting
(`TerminalTabsViewModel.kt:666-670`) is unnecessary.

**On the team lead's stale-claim-after-crash question — it does not arise.** The registry is
an in-process `Mutex<HashMap>`. Process death (jetsam, force-quit, crash) destroys it
outright, so no claim can survive into the next launch. #10's "we cannot distinguish jetsam
from force-quit from crash" property is irrelevant here: all three clear the map equally.
Worth stating that one sentence in §3.10.2-② so the next reader doesn't re-ask.

### N3 — significant — three unspecified claim semantics, one of which Android already learned the hard way

**(i) Re-claim by the same owner must be idempotent.** Not specified. If
`try_claim_tmux_window(profile, owner)` returns `false` when the map already maps
`profile → that same owner`, then a tab that reconnects and re-runs
`maybeEnsureTmuxTabWindow()` is **blocked by its own claim**, and — because the call site is
opportunistic and swallows failures (`TerminalSessionController.swift:769-771` logs a warning
and returns) — its tmux binding silently never re-establishes for the rest of the process.
→ Same owner ⇒ return `true`.

**(ii) Release on ensure-RPC failure.** Android does this explicitly, and its comment says
why (`TerminalTabsViewModel.kt:414-416`): "RPCが失敗した場合のみ解放して別タブに再挑戦の
機会を残す". The implementation is `tmuxClaimedProfileIds.remove(profile.id)` inside the
`catch` at `:1080-1083`. §3.10.2-② does not mention it. Combined with (i), a **single
transient RPC failure permanently disables tmux binding for that profile** for the process
lifetime — silently, because the whole path is opportunistic. This is a bug Android already
shipped, hit, and fixed; do not re-derive it.

**(iii) Release on teardown by any path, not just user-initiated close.** §3.10.2-② says
"claim した本人がタブを閉じるときに release する". Controller deinit, an aborted restore,
and error paths that drop a tab without going through the close flow all leak the claim.
→ Tie the release to controller teardown (`deinit` / an explicit `invalidate()`), with the
user-close path being one caller among several.

**(iv) minor, test hygiene**: a process-global `Mutex<HashMap>` needs a documented reset hook
for Rust unit tests. CI uses `cargo nextest` (process per test) so it is mostly moot there,
but `cargo test --lib` shares one process and the §3.10.4 🟡 claim tests would interfere.

---

## 5. N4 — significant — the Y-R blast radius is under-counted, and the phasing contradicts itself

D-6 item 5 and §4.4 enumerate only the **Kotlin** follow-through. Adding a method to
`OrchestratorCallback` breaks **three Swift conformances** as well:

| conformance | file | target | CI job that goes red |
|---|---|---|---|
| `TerminalSessionController` | `TerminalSessionController.swift:112` | `IsekaiTerminalCore` | `ios-rust-core-check`, `ios-app-build-check` |
| `SshVerticalSliceRecorder` | `Tests/IsekaiTerminalCoreTests/SshVerticalSliceTests.swift:61` | `IsekaiTerminalCoreTests` | `ios-rust-core-check`, `ios-ssh-vertical-slice-check` |
| `KeyManagerAuthRecorder` | `Tests/IsekaiTerminalCoreLogicTests/KeyManagerTests.swift:73` | **`IsekaiTerminalCoreLogicTests`** | **`ios-logic-linux-check`** — the ADR's own designated first gate |

Verified that this is mandatory, not optional: the generated Kotlin `OrchestratorCallback`
(`isekai_terminal_core.kt:8349+`) declares every method abstract with no default bodies, and
the Rust trait (`lib.rs:1470-1500`) declares them without default implementations. UniFFI
emits required protocol members on both sides.

**And the phasing contradicts itself.** The §4.1 diagram annotates Y-R with
「※ Y-P1 と並行実行可」, while §4.1-3 says 「Y-R を Y-P2 の前に完了させ」 and §4.2-5 says
「Y-R は並列化しない」. Y-R being a single worktree does not stop it from breaking *other*
worktrees: the moment Y-R merges, every in-flight Y-P1 branch has a Logic test target that
no longer compiles until it rebases and adds the stub. That is the parallel-worktree
friction the ADR is otherwise careful about, arriving from the one phase designed to avoid
it.

**Fix (two lines)**:
1. Add the three Swift conformances to D-6's checklist alongside the Kotlin ones.
   (Also worth correcting: `FakeSshGateway.kt` in both `src/test` and `src/androidTest` only
   *holds* `var callback: OrchestratorCallback?` — it does not implement the interface. The
   sole Kotlin implementer is the anonymous object at `TerminalSession.kt:235`. As written,
   D-6's "FakeSshGateway 2箇所" sends the executor looking in the wrong place.)
2. **Move Y-R to the front — merge it before or together with Y-P0.** Y-P0 is small
   (`SettingsView` extraction + a migration registry) and neither piece depends on the new
   APIs, so nothing is lost by putting Y-R first; and with no Y-P1 worktrees in flight yet,
   the signature change breaks nobody. The current "parallel with Y-P1" annotation buys a
   little wall-clock and costs a guaranteed cross-worktree break.

---

## 6. Answers to the open questions

**Q11 (asked directly) — yes, split Y-P2, but not by moving #4 into Y-P1.**
Moving #4 up would load Y-P1 — the one phase specified as 「配線のみ・Rust変更ゼロ・スキーマ
変更ゼロ」, i.e. the freely parallelizable one — with a GRDB migration, a `ProfileEditView`
change, and a notification-permission flow. That trades a small contention problem for a
large one. Recommended shape:

```
Y-P2   #10 のみ（TabRestoreStore + claim + 逐次復元 + TabRestoreCoordinator）
Y-P2b  #4b（経路B: NotifyGenerationTracker + TabAlertCopy + TabAlertNotifier）
Y-P2c  #4a（経路A: GRDB v7 + ensureWindow 実配線 + ProfileEditView トグル）
```

Three reasons this is better than either alternative: (a) **4b needs no schema change at all**
— it is pure Swift wiring over data that already arrives, so it is the single highest
value-per-effort unit in the plan and should not be gated behind a migration; (b) 4b touches
`TerminalSessionController.onScreenUpdate` while #10 touches
`TabRestoreCoordinator`/`TerminalTabsHostView`, so the contention between them is smaller
than between 4a and #10; (c) it lets the 🔴 manual check for claude-hookd notifications
happen early, while there is still budget to react if the wiring assumption is wrong.

**Q9 — acceptable; record it as a triggered follow-up, not an open TODO.**
The claim registry is process-local and both platforms are single-process, so two
implementations cannot interact and cannot disagree — the cost is duplicated reasoning, not
correctness. Touching working Android code purely for symmetry is an unforced risk of the
sort `parallel-worktree-agent-operations.md` warns about. Write it down with a **trigger**
("if the Kotlin set is implicated in another routing incident, migrate then") rather than an
open-ended intention, so it does not sit as a permanent reproach.

**Q10 — keep Android at no-op-plus-log in Y-R.**
Y-R is the one PR whose correctness gates two required checks (`android-uniffi-drift`,
`android-unit-test`). It should be as surgical as possible. Adding Android banner UX to it
makes a mechanical binding change into a UX change, and the two would then fail review
together. Ship Android consumption of `on_foreground_resume` as a separate follow-up if it
turns out to be wanted.

**Q1 (remaining) — endorsed.** With ownership moved to Rust, what is left in Swift really is
presentation order. The boundary as stated (promote-on-tap = presentation; "give up on tab
N" = session state → Rust) is the right line and is already written down. No change needed.

**Q2 — keep "no nag".** The proposed weak nag couples two causally unrelated signals: low
power mode does not cause resume-reconnect failures, so a message conditioned on their
conjunction would misattribute the problem — the same defect as mapping
`!isLowPowerModeEnabled` into `is_ignoring_battery_optimizations`, which §3.9.4-A already
rejects. If repeated resume failures are worth surfacing, surface them on their own merits,
and put the decision in Rust as the ADR says.

**Q4 / Q6** — no new opinion; both remain reasonable as deferred.

---

## 7. Minor

- **m2 — "B-1/B-2/B-3" now means two different things.** #1 uses it for release stages
  (§3.8) and #6 uses it for spike outcomes (§3.7, R12). This document will be handed to
  parallel agents as per-item task briefs, where 「B-1 だけ実装する」 is genuinely ambiguous.
  Rename one set (e.g. #6 → Outcome-1/2/3).
- **m3 — §3.9.3 block ④ is now correct** (S5 closed). It says tabs *are* restored and that
  the reconnect is always cold-start, which matches §3.10.2-③. No further change.
- **m1** — see §2 above (bound the Swift-side gates to exactly two).

---

## 8. Fresh premortem — failure modes introduced *by the round-2 fixes*

The round-1 premortem scenarios P1–P5 are all now mitigated in the document (R3–R6, R8, R11
and the reorder). These four are new, and every one of them is a consequence of a fix rather
than of the original plan.

### NP1 — "Y-R merged and every other worktree went red."
Y-R runs in parallel with Y-P1 as the §4.1 diagram permits. It lands the
`OrchestratorCallback` method. Three Swift conformances in three different targets now fail
to compile — including `KeyManagerTests` in `IsekaiTerminalCoreLogicTests`, so
`ios-logic-linux-check` (the cheap first gate every other item relies on) is red on branches
that changed nothing. Each in-flight Y-P1 worktree must rebase and add a stub before it can
see its own test results. Because iOS jobs are not required checks, this can also be
*merged past* and left broken on `main` for a while.
*Prevented by*: **N4** — Y-R first, and the three Swift conformances on the checklist.

### NP2 — "AI notifications work, then stop after the first reconnect."
`NotifyGenerationTracker` ships as a naive monotonic comparison. Everything passes: the
Logic tests cover the five listed cases, and the 🔴 manual check passes because it is run on
a fresh session. In daily use the first Wi-Fi blip or foreground resume creates a new
Terminal, the counter restarts below the remembered high-water mark, and every subsequent
claude-hookd notification is dropped. Intermittent and session-dependent — the hardest shape
of P1 to diagnose, and #10 makes the triggering event routine rather than rare.
*Prevented by*: **N1** — reset on Connected, synchronously, in the documented MainActor
context.

### NP3 — "tmux binding quietly stopped working on that host and never came back."
A transient `ensureTmuxTabWindow` RPC failure (server mid-restart, tmux briefly absent)
leaves the claim held by a tab that owns no window. Because re-claim by the same owner
returns `false` and nothing releases on failure, neither that tab nor any other can ever
retry for the life of the process. The call site logs a warning and returns — by design,
it is opportunistic — so nothing surfaces. The user sees clipboard sync and window
targeting silently degrade with no event to point at. This is P3 wearing the fix's clothes:
the guard that was added to prevent corruption becomes the thing that prevents recovery.
*Prevented by*: **N3(i)+(ii)** — idempotent re-claim, release on failure. Android's
`catch` block is the reference implementation.

### NP4 — "The honesty screen shipped a new false statement."
§3.9 exists because Android's copy would be misleading on iOS, and §3.9.3's acceptance
conditions test that "Background App Refresh" never appears and that block ④ no longer
says 「復元されません」. But the two *new* strings introduced in round 2 are unguarded:
「復帰しました（接続は維持されています）」 shown over a tab that was never connected (N2a),
and 「再接続しました」 shown a moment before a synchronous reconnect failure emits
`Disconnected` (N2b). The regression tests added for S5 pin the old lies and not the new
ones.
*Prevented by*: **N2** — gate on `background_state != Foreground`, and make the flag mean
"initiated" with copy to match. Worth extending the 🟢 copy tests to cover the resume banner
strings, not only the `BackgroundBehaviorView` blocks.

---

## 9. Summary

| ID | Severity | Item |
|---|---|---|
| — | — | **S2 conceded**: `IsekaiTerminalCore-Package` does run `IsekaiTerminalCoreTests`; architect correct. Deterministic call-count gate adopted and endorsed |
| B1 | **resolved** | path A/B split, `TabAlertCopy` signature, `message` branch as acceptance condition |
| B2 | **resolved** | D-1 two-column definition + `TabAlertNotifier.kt:26-31` precedent |
| B3 | **resolved** | `on_foreground_resume(did_reconnect:)`, 2-way alternative correctly rejected |
| B4 | **resolved** | Rust claim, owner-matched release, no cross-restart staleness (in-process map) |
| N1 | significant | `NotifyGenerationTracker` reset: synchronous, on Connected (not only manual `reconnect()`) |
| N2 | significant | `on_foreground_resume`: don't fire when untracked; flag means *initiated*; per-tab vs app-level banners |
| N3 | significant | claim: idempotent same-owner re-claim; release on RPC failure (Android precedent); release on any teardown; test reset hook |
| N4 | significant | Y-R breaks 3 Swift conformances incl. the Logic test target; move Y-R ahead of Y-P0/Y-P1 |
| Q11 | answered | split Y-P2 → #10 / #4b / #4a; do **not** move #4 into Y-P1 |
| Q9 | answered | acceptable; record as a *triggered* follow-up |
| Q10 | answered | Android stays no-op in Y-R; keep that PR surgical |
| Q1, Q2 | answered | endorsed as written |
| m1–m3 | minor | bound Swift-side gates to exactly two; rename #6's B-1/B-2/B-3; block ④ now correct |

**No blocking issues remain.** Fold N1–N4 in and close the loop.
