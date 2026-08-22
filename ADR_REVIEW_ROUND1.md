# ADR_IOS_PARITY_IMPLEMENTATION.md — Round-1 Adversarial Review / Premortem

- **Reviewer**: reviewer agent (Opus), 2026-08-21
- **Target**: `/home/cuzic/isekai-terminal/ADR_IOS_PARITY_IMPLEMENTATION.md`（round-1 draft）
- **Grounding**: `IOS_PARITY_GAP.md`, the ADR, `CLAUDE.md`, `.claude/rules/{rust-ssot,uniffi-binding-regeneration,always-connects,parallel-worktree-agent-operations,main-branch-protection}.md`,
  `PLAN.md` §Phase Y（1780行〜、特に Phase 1C #24 の 2796-2868行）, and direct verification against the source tree.

---

## Overall assessment

This is a strong draft. The §1.2 corrections table is real work — I independently confirmed
every one of its five corrections against source:

| §1.2 claim | verification |
|---|---|
| #2 form submit is pure Kotlin, no Rust API | ✅ `TerminalSession.kt:212-218` — `org.json.JSONObject(values).toString()` + `send()`. `ai_panel.rs` exports nothing (`pub(crate)` only) |
| #4 `enableNotifications` hardcoded `false` | ✅ `TmuxTabWindowCoordinator.swift:107-111` doc + `enableNotifications: Bool = false` default; sole caller `TerminalSessionController.swift:761` passes nothing |
| #4 `notifyFocusChange` already wired on iOS | ✅ `TerminalView.swift:185,193` → `TerminalSessionController.swift:536` |
| #7 is `SharedPreferences`/`AppSettingsKeys`, not GRDB | ✅ `AppSettingsKeys.swift` has exactly 4 keys today |
| #9 `decideBatteryGuidance` generated but Android-shaped | ✅ `BackgroundKillFacts` at generated `isekai_terminal_core.swift:1976` |
| #10 no `ReattachStateStore` equivalent; `reattach_*` already exported | ✅ zero grep hits in `ios/Sources`; `reattach_persistence.rs:63-77` `#[uniffi::export]`, `AUTO_REATTACH_GRACE_SECS = 30*60` |
| lifecycle fanout already exists | ✅ `TerminalTabsHostView.swift:63-125` |

The §3.9/§3.10 reasoning about iOS's real constraints is correct and does **not** import
Android assumptions. In particular §3.9.2's rejection of Background App Refresh as 誤誘導,
§3.9.4-A's rejection of mapping `!isLowPowerModeEnabled` into
`is_ignoring_battery_optimizations`, and §3.10.5-D's observation that "server keeps the PTY
alive" is just tmux and already shipped — those three are the best calls in the document.

My findings below are mostly about what the ADR is **silent** on, not what it got wrong.
Four are load-bearing enough to block.

---

## Findings

### B1 — blocking — #4 covers only one of the two notify paths, and misses the one that matters most here

There are two independent notify mechanisms in this codebase:

- **Path A (tmux hooks)**: `OrchestratorCallback::on_notify(kind)` — Rust-suppressed via
  `notify_focus_change`, dedup'd by `(tmux_tag, seq)` in `orchestrator.rs:657`. iOS no-op at
  `TerminalSessionController.swift:990`. This is the one §3.5 designs for.
- **Path B (ctl `Notify`)**: `notify_generation` / `notify_kind` / `notify_title` /
  `notify_body` on the screen-update struct (`rust-core/src/lib.rs:1199-1206`). Android diffs
  the generation inside `TerminalSession` and calls `onNotify(kind, title, body)` →
  `TabAlertNotifier.notify(..., message = title to body)` (`TerminalTabsViewModel.kt:286-296`).

On iOS, `TerminalScrollback.swift:47-50` already copies
`notifyGeneration`/`notifyKind`/`notifyTitle`/`notifyBody` into the snapshot and **nothing
consumes them**. Path B is what `isekai-pipe ctl notify` and claude-hookd drive — i.e. the
notifications the repo owner actually uses daily.

Worse, §3.5 specifies `TabAlertCopy.swift` as a pure `NotifyKind → (title, body)` map.
Android's equivalent (`titleAndTextFor`) is only the **fallback** for path A; path B
deliberately overrides it, and `TabAlertNotifier.kt`'s doc says why: 固定文言で上書きすると
「送出側の意図が失われるため」. Implementing §3.5 as written would either never fire AI
notifications at all, or fire "完了 / リモート側の処理が完了しました" while silently discarding
the real title/body.

**Fix**: split #4 into 4a (path A: GRDB v7 + `ensureWindow` wiring + `onNotify`) and 4b
(path B: `notifyGeneration` diff consumer + permission + delivery). Give `TabAlertCopy`
Android's actual signature — `(kind, profileLabel, message: (String, String)?) -> (String, String)`
— and test the override branch, not only the 7-case fallback.

---

### B2 — blocking — D-1's "Swift never suppresses" is too absolute and would over-deliver notifications

§3.5 says `willPresent` must "Rustが送ってきたものは常に表示する" and that any Swift-side gate
is a `rust-ssot.md` violation. That is not what Android does, and Android's own doc says so
explicitly (`TabAlertNotifier.kt:26-31`):

> ここで責任を持つのは次の2点だけ(`.claude/rules/rust-ssot.md`: セッション状態に基づく判断は
> Rust側、純粋なUI opt-in設定・OS権限確認はKotlin側でよい): 1. このタブ(プロファイル)自身の
> 通知opt-in設定 2. `POST_NOTIFICATIONS` 実行時権限

`TabAlertNotifier.notify` opens with `if (!enabled) return; if (!hasPermission(context)) return`.

The profile flag *cannot* be enforced Rust-side for path B — it never reaches Rust; it only
gates tmux hook installation for path A. So a Swift build following D-1 literally would
deliver AI notifications from profiles whose toggle is **off**.

**Fix**: restate D-1 as "no *session-state-derived* suppression in Swift — pure UI opt-in and
OS permission gating stay Swift-side", and cite `TabAlertNotifier.kt`'s doc as the precedent
so a later reviewer doesn't flag it as a violation. The rest of D-1 (no `BackgroundState`
mirror enum, no hardcoded 30分/2回 thresholds) is correct and worth keeping verbatim.

---

### B3 — blocking — §3.9.3(c)'s three-way resume banner is not implementable under the "zero Rust API" premise

`BackgroundState` is private, and its doc comment is explicit (`orchestrator.rs:180-183`):

> UniFFIへは公開しない(Kotlin/Swiftは生イベントを送るだけでよく、この状態自体を読んで分岐しては
> いけない、`rust-ssot.md`)

And in the "returned within grace, connection still alive" case,
`notify_will_enter_foreground` (`orchestrator.rs:1285-1300`) takes the `was_suspended == false`
branch and **emits no callback at all**.

So §3.9.3(c)'s claim「この3分岐の判断は Rust 側が既に持っている… Swift は結果を描画するだけ」
is only half true: Rust holds the decision but exposes no way to observe which branch it took.
Swift could only infer it by watching for a `Connecting` state change shortly after
foregrounding — which is a mirror state machine, racy against a user-initiated reconnect, and
precisely what D-1 forbids.

This is the ADR's headline conclusion (「Rust側の public API 追加は1つも必要ない」) failing on
its own terms, and §4.4's exception list does not include it.

**Fix**: pick one and say so.
(a) Drop to a two-way banner driven only by existing `on_connection_state_changed` transitions
plus the cold-start flag `TabRestoreCoordinator` already owns; or
(b) add exactly one callback — `on_foreground_resume(did_reconnect: Bool)` — and move it into
§4.4's trigger list and into the batched regen (S1).

---

### B4 — blocking — #10's multi-tab restore reintroduces a bug Android already shipped and fixed

`tmux_tab_locators` is **primary-keyed by `profileId`** — one tag per profile
(`ProfileDatabase.swift:509-513`, whose own comment says 「`profileId`が主キー(1プロファイルに
つき高々1タグ)」; Android `TmuxTabLocator.kt:30`). So §3.10.2-①'s
「tmux ウィンドウの紐付けは既存の GRDB `TmuxTabLocator` テーブルが既に持っているので重複して
持たない」silently breaks for the N-tabs-on-one-profile case — which is the exact case #10
exists to serve.

Android handles this with `tmuxClaimedProfileIds` — a `ConcurrentHashMap.newKeySet<Long>()`
reserved via `putIfAbsent` **before** launching the coroutine, released only when the last tab
for that profile closes (`TerminalTabsViewModel.kt:418`, `666-670`). Its doc records the actual
incident: two same-profile tabs racing to `connected` corrupted ctl-socket routing so that
`@isekai_ctl_sock`「永久に正しいウィンドウへ届かなくなる二次被害があった」.

`grep -rn "claimed" ios/Sources` returns **zero** hits. `maybeEnsureTmuxTabWindow()`
(`TerminalSessionController.swift:758-773`) fires unguarded, once per controller.
§3.10.2-④'s sequential restore turns a two-tab race into an N-tab one, at cold start, where
the user has no way to correlate cause and effect.

**Fix**: add the claim guard to the #10 design explicitly, plus an acceptance criterion
("2 tabs on the same profile restore; exactly one claims the tmux window; closing it hands the
claim to the other"). Also decide **where** it lives — this is arguably the strongest argument
in the whole ADR that Q1's answer should be "Rust": 「which session owns the tmux window for
this profile」is session state, not presentation order, and Kotlin ownership of it is exactly
what broke before.

---

### S1 — significant — no batching policy for the regen round-trip, and §4.4 omits the Kotlin side

Given B3 (and possibly B4), Rust API additions are likelier than the ADR assumes. §4.4 lists
*triggers* but no *cadence*, which invites one regen per API — each one a
workflow_dispatch → wait → download → copy 6 files → diff cycle.

Also: §4.4 names only the three Swift files plus three `.sha256` sidecars.
`regenerate-uniffi-bindings.yml` also emits the Kotlin binding, and **`android-uniffi-drift`
is a required status check on `main`** (`.claude/rules/main-branch-protection.md`). An
iOS-motivated Rust API addition that forgets
`android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt` turns a required
check red and blocks everyone's merges — for a change no Android code even uses.

**Fix**: add to §4.4 — "Rust API additions are accumulated and regenerated once, at a named
checkpoint (end of Y-P3); never one per PR" — and add the Kotlin binding to the file list with
the required-check consequence spelled out.

---

### S2 — significant — #3's acceptance criterion targets a test suite CI never runs

§3.6 gates on 「`TerminalFrameRendererTests` にフレーム時間の回帰計測を追加」. That file
(`ios/Tests/IsekaiTerminalCoreTests/TerminalFrameRendererTests.swift`) lives in
`IsekaiTerminalCoreTests`, the Apple-only SwiftPM test target. Where that target actually runs:

- `ios-ssh-vertical-slice-check.yml:115` — `-only-testing:IsekaiTerminalCoreTests/SshVerticalSliceTests`
  (that one class only).
- `ios-app-build-check.yml:126-129` — `xcodebuild test -scheme IsekaiTerminalApp`, and that
  scheme's only test blueprints are `IsekaiTerminalAppTests` / `IsekaiTerminalAppUITests`
  (verified in `IsekaiTerminalApp.xcscheme`).

So `TerminalFrameRendererTests` executes in **no** CI job today. Adding the perf gate there
means writing a gate that never fires — and R1's mitigation in §5.3 is therefore fictional
as written.

**Fix**: either add `IsekaiTerminalCoreTests` to the app scheme (a real, separately reviewable
change), add a second `-only-testing:` invocation, or move the measurable part into the Logic
target. Whichever — say it in the ADR.

---

### S3 — significant — #10's acceptance test is in the wrong target, and tests the wrong kind of kill

§4.3 puts the `simctl terminate` → relaunch test in `IsekaiTerminalAppTests`. That is an
in-process, host-app unit target; it cannot terminate and relaunch itself. Terminate+relaunch
requires `XCUIApplication.terminate()` / `.launch()` from `IsekaiTerminalAppUITests` — which
currently contains exactly one file, `AppLaunchUITests.swift`.

Second: `XCUIApplication.terminate()` is a *graceful* kill. It exercises "records survive and
restore fires", which is the half the design already assumes; it does **not** exercise jetsam.
That's an acceptable limitation — but the ADR should say the jetsam half stays uncovered rather
than implying acceptance criterion 1 is fully automated.

---

### S4 — significant — no verification story for the UI-heavy items

#1 (file preview) and #2 (AI panel) are the two largest UI deliverables and the ADR says
nothing about how they get checked beyond Logic-target unit tests. With one launch-smoke
UITest in the repo and `ios-app-build-check` **not** a required check, "done" for these items
currently means "an agent said it looks right".

**Fix**: per item, state what is CI-verified (name the Logic tests) versus what is an explicit
user visual check, and put the latter into the acceptance conditions. Given this repo's
documented history of an agent reporting passing tests that weren't, making the manual step an
explicit deliverable is worth the two lines.

---

### S5 — significant — §3.9.3 block ④ contradicts §3.10.2-③, and would ship false copy to users

Block ④'s user-facing text:「アプリスイッチャーから上スワイプで終了すると、次回起動時の自動復元が
されません」.

§3.10.2-③: records remain unless the user explicitly closed tabs;「起動時にレコードが残って
いれば、jetsam・force-quit・クラッシュのいずれかである」→ **restore fires**, force-quit
included, and the ADR says the three are deliberately indistinguishable.

Under the ADR's own design, block ④ is false. On a screen whose entire thesis is 率直な説明
that doesn't mislead (§3.9.2-2 rejects Background App Refresh precisely for being 誤誘導),
shipping one wrong sentence is self-defeating.

**Fix**: replace block ④ with something true — swipe-kill drops the live connection immediately
with no grace-window resume, and (unlike a normal background transition) guarantees a
cold-start reconnect. Or delete the block.

---

### S6 — significant — record freshness will expire under a long foreground session

Android refreshes `savedAtUnixSecs`「タブを開いた時点、および接続が Connected へ遷移するたびに」
(`ReattachStateStore.kt`, `ReattachRecord.savedAtUnixSecs` doc). §3.10.2-① writes only at
open/close and `didEnterBackground`.

Concrete failure: a tab open and actively used for two hours, then a foreground crash or OOM
while still active. The record's timestamp is two hours old, `reattachRecordIsFresh` returns
false, nothing restores — after the *most* engaged session, which is exactly when the user
most wants restoration.

**Fix**: mirror Android — also refresh the timestamp on transition to Connected. Cheap, and it
removes a whole class of "why didn't it restore?".

---

### S7 — significant — `ProfileListView.swift` is the second contention file, unflagged

§4.2 flags `TerminalSessionController.swift` (correct — #5/#2/#4/#1 all touch it). But every
app-wide setting is an `@AppStorage` toggle inlined in `ProfileListView.swift:55-58`; there is
**no `SettingsView.swift`** in the tree. #7 (Y-P1), #3's font-import entry point, #6-B2's
layout picker, and #9's `BackgroundBehaviorView` entry point all need to land there.

Also, §3.9.3(b)'s「設定メニューから常時開ける」presumes a settings menu that does not exist as
a screen. That is unscoped work.

**Fix**: add `ProfileListView.swift` to §4.2's contention list, and decide up front whether to
extract a `SettingsView` as a Y-P1 prerequisite (my recommendation — it is small now and four
later items depend on it) or to keep accreting toggles in `ProfileListView`.

---

### S8 — significant — #1's genuinely hard part isn't in its Logic list

§3.8 lists display-model derivation, file-type sniffing, CSV parsing, path joining. The part
most likely to leak or hang is the async request/response correlation: Android keeps
`ConcurrentHashMap<String, CompletableDeferred<FilePreviewOutcome>>` keyed by `requestId`
(`TerminalSession.kt:232`), because `file_preview_request` queues and returns immediately with
results arriving later on `onFilePreviewResult`. Multiple `ls`/`cat` requests can be in flight
simultaneously.

Not mentioned anywhere: the in-flight registry, cancellation when the tab closes or the sheet
is dismissed mid-request, behavior when the session disconnects with requests outstanding
(`orchestrator.rs:3408` covers the *not-connected-at-request-time* path; mid-flight disconnect
is a different case), and chunk reassembly for `cat`.

**Fix**: name the registry as a Logic-testable unit with an injected transport, and add
"disconnect with N requests in flight resolves all of them" to B-1's acceptance conditions.

---

### S9 — significant — the ADR builds on PLAN.md Phase Y without noting where Phase Y is stale

§1.3 cites Phase Y as settled ground. But Phase Y's Phase 1C section (`PLAN.md:2796-2868`)
describes `rust-core/src/session_supervisor.rs` — an 8-state `SessionState` × `ExecutionMode`
FSM exposed as a UniFFI Object — as the live design, and states that integration with
`SessionOrchestrator` is「未実施のまま次フェーズ以降へ持ち越す」.

That file no longer exists. Commit `710aecf2` deleted it and folded it into `orchestrator.rs`'s
private 3-state `BackgroundState`; `orchestrator.rs:178-186` documents the reduction
(`Closing`/`Closed` dropped, `Connecting`/`Resuming` already covered by `ConnPhase`,
deliberately **not** UniFFI-exposed).

The ADR uses the *current* reality and is right to. But it never says Phase Y is superseded
here, so the next reader reconciling the two documents has to re-derive this from scratch —
the same staleness that `IOS_PARITY_GAP.md`'s own preamble complains about, one layer deeper.

**Fix**: one paragraph in §1.3 naming exactly which Phase Y statements are superseded
(`SessionSupervisor` 8-state FSM → `BackgroundState`; `session_supervisor.rs` deleted in
`710aecf2`), and add "update PLAN.md Phase Y" as an explicit deliverable of the final phase
rather than leaving PLAN.md to rot further.

---

### S10 — significant — answering Q3 / Q5: prefer the reservation script over serializing on D-4

**Q3**: keep `enableTabNotifications` **profile-scoped**. Per-profile is the right granularity
(notify from the build-server profile, not the scratch box), it matches Android, and it is the
value Rust needs at `ensureWindow` time to decide on hook installation. Accept GRDB v7.

**Q5**: build the GRDB reservation registry now, and drop D-4's serialization. D-4 buys safety
only while exactly one migration exists — and it is already conditional
(「他の項目が GRDB スキーマ変更を必要とすることが判明した場合は、実装を始める前に本ADRを改訂して
順序を直列化する」), which is a promise to re-plan mid-flight. Given #3 could plausibly want a
per-profile font and #1 a preview-history table, the odds that D-4 survives contact are not
high. `scripts/reserve-room-migration.sh` + `migration_registry.toml` + a check workflow is a
small, already-proven pattern; porting it removes an ordering constraint from the whole plan
instead of just this one item.

---

### M1 — minor — §3.2's "key ordering determinism" test isn't a parity property

Android does `org.json.JSONObject(values).toString()`, whose ordering follows the passed map's
iteration order — not a specified contract. Testing determinism on iOS is fine; just don't
frame it as matching Android.

---

### M2 — minor — give the #6 spike a falsifiable target, and expect a third outcome

§3.7 offers a binary (auto-detect possible / not possible). There is a likely middle case worth
naming: `UIKeyboardHIDUsage` exposes JIS-only physical keys — `keyboardLANG1`/`keyboardLANG2`
(かな/英数) and `keyboardInternational1`/`keyboardInternational3` (ろ/¥). If those surface
through `GCKeyboard` / `pressesBegan`, *reactive* detection is plausible (default to US, flip
to JIS on the first JIS-only keypress) even with no upfront layout API. That is a materially
better outcome than Phase B-2 and a materially different design.

I cannot verify device behavior from this sandbox — which is exactly the point: make
"do these HID usages actually arrive from a real JIS Bluetooth keyboard?" the spike's explicit,
falsifiable question rather than leaving it to a literature search.

---

### M3 — minor — §3.9.3 block ③ needs a field §3.10.4 doesn't define

「直近の復帰: 12分前に3タブを復元しました」requires a persisted last-restore timestamp + count.
`TabRestoreRecord` as specified (`tabId` / `profileId` / `savedAtUnixSecs` / `isActive`) has
nowhere to put it. One field — but #9 is the last item, so discovering it then means reopening
#10's already-shipped store format.

---

### Items already in good shape (no critique)

- **#5, #7, #8**: checked against source, nothing to add. The API surface exists as claimed
  (`on_prompt_jump` / `on_prompt_output_copy_ready` at `lib.rs:1497-1500`,
  `copy_last_command_output` at `orchestrator.rs:1420`), the `AppSettingsKeys` correction is
  right, and §3.3's reasoning for keeping snippet templates out of Rust is sound.
- **§3.4's security note**: insisting that fingerprint mismatch stays rejected regardless of
  the toggle is exactly right per `.claude/rules/always-connects.md`, and correctly identifies
  it as intentional design rather than a gap.
- **§3.9.4-A and §3.10.5-D**: the two best calls in the document.

---

## Premortem — the 5 most likely ways this plan goes wrong

### P1 — "We shipped notifications and my Claude notifications still don't arrive."

#4 ships per §3.5. Path A works: tmux bell/activity reach the phone. Path B was never
implemented — `notifyGeneration` still sits unread in `TerminalScrollback`. The one
notification stream the repo owner actually depends on (claude-hookd →
`isekai-pipe ctl notify`) is silent, and because *some* notifications work, diagnosis is slow:
it reads as a suppression bug, not a missing feature.

Second-order: if a partial fix wires path B through `TabAlertCopy` as spec'd, notifications
arrive with generic fallback text and the real title/body silently dropped — worse than
silence, because it looks like it's working.

*Prevented by*: **B1 + B2**.

### P2 — "A one-line iOS callback turned `main` red for everyone."

Y-P4 hits B3, needs `on_foreground_resume`, and the ADR's「Rust変更ゼロ」framing means nobody
planned the round-trip. Someone triggers `regenerate-uniffi-bindings.yml`, copies the three
Swift files, and forgets the three `.sha256` sidecars (documented as having actually happened
on 2026-08-09) and/or the Kotlin binding. `android-uniffi-drift` is a required check on `main`;
every parallel PR is now blocked by a check none of them caused, in the middle of
parallel-worktree operation, and the fix requires another full CI round-trip.

*Prevented by*: **S1** (batching policy + Kotlin binding in the file list), and by resolving
**B3** before implementation starts rather than during it.

### P3 — "After the update my tmux windows are scrambled and clipboard sync stopped working."

#10 ships. A user with three tabs on one host gets jetsam'd, relaunches, and the sequential
restore drives three unguarded `maybeEnsureTmuxTabWindow()` calls against a profile-keyed
locator with no claim reservation. This is the same race whose Android fix comment records
「`@isekai_ctl_sock`が永久に正しいウィンドウへ届かなくなる二次被害」— now triggered at cold
start, where the user cannot connect cause to effect, and where the corrupted routing persists
across restarts. The bug reads as "clipboard sync is broken", not "restore is broken", so it
gets filed against the wrong subsystem.

*Prevented by*: **B4**.

### P4 — "All acceptance criteria met" on code no CI ever executed.

The ADR reads as rigorous — every item has 受け入れ条件. But #3's gate lives in a suite CI never
runs (S2), #10's lives in the wrong target and cannot compile as described (S3), #1/#2 have no
verification story at all (S4), and `ios-app-build-check` isn't a required check so even the
parts that do run are guarded only by「PRマージ前にgreenを目視確認する運用」. Combined with
this repo's documented history of an agent reporting passing tests that weren't, the plan
completes on paper while several items are unverified — discovered later, on device, in a batch.

*Prevented by*: **S2 / S3 / S4** — make each acceptance criterion name the CI job that executes
it, and mark the manual ones as manual.

### P5 — "Y-P4 never happened."

Y-P1 through Y-P3 are eight tractable items with visible output. #9/#10 are the two hard design
items, placed last, and they are the *only* ones addressing the complaint that actually drives
users away ("Android stays connected, iOS drops"). Eight items of momentum consume the attention
budget; #10 slips; #9 depends on #10 so it slips too; the parity effort gets declared "mostly
done" with the one thing users notice untouched. The ADR raises this itself as Q8 and then
doesn't act on it.

*Mitigation*: move **#10 to immediately after Y-P1**. It has zero file contention with Y-P2
(#4/GRDB) or Y-P3 (#3/#1), all its dependencies are satisfied today (the `reattach_*` Rust
policy and the lifecycle fanout both already exist), and it is the only item that unblocks
another (#9). The current ordering is justified by nothing stronger than "easy things first" —
a good default, and the wrong one here.

---

## Summary of requested ADR changes

| ID | Severity | Change |
|---|---|---|
| B1 | blocking | Split #4 into path A / path B; `TabAlertCopy` must take the sender-supplied `message` override |
| B2 | blocking | Reword D-1: profile opt-in + OS permission gating are legitimately Swift-side |
| B3 | blocking | Resolve the 3-way resume banner: 2-way from existing callbacks, or add `on_foreground_resume(did_reconnect:)` to §4.4 |
| B4 | blocking | Add a per-profile tmux claim guard to #10; decide Swift vs Rust (feeds Q1) |
| S1 | significant | Add regen batching policy; add the Kotlin binding + `android-uniffi-drift` consequence to §4.4 |
| S2 | significant | Fix #3's acceptance target — `IsekaiTerminalCoreTests` runs in no CI job today |
| S3 | significant | Move #10's restore test to `IsekaiTerminalAppUITests`; note jetsam stays uncovered |
| S4 | significant | Add per-item CI-vs-manual verification statements, especially #1/#2 |
| S5 | significant | Fix the §3.9.3-④ vs §3.10.2-③ contradiction (false user-facing copy) |
| S6 | significant | Refresh `savedAtUnixSecs` on Connected, matching Android |
| S7 | significant | Add `ProfileListView.swift` to §4.2 contention; decide on extracting `SettingsView` |
| S8 | significant | Add the `requestId` in-flight registry / cancellation / chunking to #1's Logic scope |
| S9 | significant | State which PLAN.md Phase Y claims are superseded; make "update PLAN.md" a deliverable |
| S10 | significant | Answer Q3 (keep profile-scoped) and Q5 (build the GRDB reservation registry, drop D-4) |
| M1 | minor | Don't frame JSON key ordering as an Android parity property |
| M2 | minor | Give the #6 spike the `keyboardLANG1`/`International1` hypothesis as its falsifiable target |
| M3 | minor | Add a last-restore timestamp/count field to `TabRestoreRecord` for §3.9.3-③ |
| P5 | — | Reorder: move #10 to immediately after Y-P1 |
