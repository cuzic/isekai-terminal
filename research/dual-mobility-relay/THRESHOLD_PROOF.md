# The (β1, β2) switching-curve derivation

This extends `STAGE0_REPORT.md`/`FORMALIZATION_REVIEW.md`'s remaining open item: deriving
the two switching curves analytically instead of reading them off noisy Monte Carlo
heatmaps, and proving threshold structure where possible. Code: `dmr/beliefgrid2d.py`,
`dmr/switching_curves.py`; driver: `switching_curves_demo.py`; figure: an Artifact built
via the dataviz skill (see the repo's session notes for the URL).

Every claim below marked "verified" was checked in code before being trusted, per this
project's convention — see the referenced scripts to reproduce.

## 0. Setup

Two Gilbert-Elliott hops (`hop1`, `hop2`), joint state `s = (s1, s2) ∈ {G,B}²`, indexed
`s1*2+s2` (`GG=0, GB=1, BG=2, BB=3`, matching `dmr/channels.py`). Routing action `a ∈
{A,B}`; warm-standby maintenance action `m ∈ {COLD,WARM}` (`dmr/warm_standby.py`'s 4
combined actions). `rho` is the inter-hop correlation knob; **everything in this note is
`rho=0` only** (independent hop dynamics).

## 1. The (β1, β2) reduction (verified numerically, `dmr/beliefgrid2d.py`)

**Claim 1.** At `rho=0`, `channels.joint_transition_matrix(hop1,hop2,0) = kron(T1,T2)`
exactly (`T1`,`T2` the two hops' own 2x2 transition matrices), and
`decomposed_obs_likelihood` factors as `p1(l1|s1)·p2(l2|s2)`. A product prior `b1⊗b2`
therefore stays a product after `predict` (the standard Kronecker identity
`(T1⊗T2)(b1⊗b2) = (T1 b1)⊗(T2 b2)`), and stays a product after a decomposed-observation
Bayes update too: the unnormalized joint posterior factors as `[b1_pred·p1(l1|·)] ⊗
[b2_pred·p2(l2|·)]`, and the normalizer itself splits as `Z1·Z2`, so the *normalized*
posterior is `b1_post ⊗ b2_post`. Verified to ~1e-16 over repeated predict/update steps
with random observations; the same check at `rho=0.6` shows factorization breaking down
(~0.02 error), confirming this is specific to `rho=0`.

**Claim 2.** Composite observation cannot preserve this. With `u_i=1-e1(s_i)`,
`v_j=1-e2(s_j)`, the loss-likelihood matrix is `M[i,j] = 1 - u_i v_j`, and
`det M = -(e1_bad-e1_good)(e2_bad-e2_good)` — nonzero (hence rank 2, hence not
separable as `f(s1)g(s2)`) *exactly whenever both hops are actually informative*. Only
the `o=0` (no-loss) branch is separable (`(1-e1)(1-e2)`, rank 1); entanglement is
concentrated exactly on loss events — the moments a decision is actually needed. This
is a sharper statement than "generic parameters," found during external review.

**Consequence.** The sufficient statistic collapses from a 4-state joint belief (a
3-simplex) to two independent scalars `β1=P(hop1=Bad)`, `β2=P(hop2=Bad)`, for the
decomposed-observation model only. `dmr/beliefgrid2d.py` implements a regular-grid RVI
solver over `(β1,β2,p,w)` exploiting this — validated three ways (`beliefgrid2d_demo.py`):
(a) reproduces `dmr/beliefgrid_warm.py`'s general 4-state simplex solve at `rho=0`,
reaching a *tighter* lower bound (`g=0.06848` vs `g=0.06576`) at less compute (10 201
grid points vs 969, since the domain is a square, not a triangulated simplex); (b) an
extreme-parameter closed-form check (forced always-absorbing-to-A policy) matches to
5.6e-17; (c) a Monte Carlo rollout that tracks the *true* general 4-state joint belief
(not artificially forced into product form) and only *projects* onto `(β1,β2)` for
action lookup matches the solver's `g` within 0.44 standard errors — i.e. the
factorization holds along real trajectories, not just for one step.

## 2. P1 — value function monotonicity (full unconstrained model)

**Proposition (P1).** For the full 4-action model (`dmr/beliefgrid2d.py`'s
`belief_grid2d_value_iteration_warm`), the relative value function `h(β1,β2,p,w)` is
nondecreasing in each of `β1,β2` (holding the other and the context fixed).

**Proof.** Each hop is a 2-state chain, so MLR order over a single hop's belief
coincides with plain scalar order on `β_i` (any 2-point distribution pair is trivially
likelihood-ratio ordered). The "persistent channel" condition `1-p_gb ≥ p_bg` (any
realistic bursty GE channel) makes the predict map `β_i' = β_i(1-p_bg)+(1-β_i)p_gb`
monotone nondecreasing in `β_i`; the Bayes update `β_i ↦ posterior` is monotone in the
prior for *any* likelihood in the 2-state case (posterior odds = prior odds ×
likelihood ratio), and monotone in the observation itself iff `eps_bad ≥ eps_good`
(both standard facts, Lovejoy 1987 / Krishnamurthy ch. 2). Immediate cost is
nondecreasing in each `β_i` (path B's loss `e1+e2-e1e2` is nondecreasing in each
argument; path A's cost is constant). Induct on RVI: if `h_n` is monotone, then for
every fixed action `k`, `content(β,k) + E[h_n(next β)]` is monotone (composition of
monotone maps, and the expectation over `(l1,l2)` is a nonnegative-weighted sum of
monotone terms since observability is a function of `(a,m)` only — never of `β` itself
— so there's no case-split that could break monotonicity). The pointwise minimum over
actions of monotone functions is monotone. RVI's per-iteration constant subtraction
(pinning the reference point) doesn't affect monotonicity. Base case `h_0=0` trivial.∎

**Verified** (`switching_curves_demo.py`): zero violations of `h` monotonicity in either
coordinate, in all 4 `(p,w)` contexts, at grid resolution 100.

**Lemma A (monotone filtering, stated explicitly since §3 reuses it precisely).** Let
`F: [0,1]² → ℝ` be nondecreasing in the product order. Fix an observation branch
`l_i ∈ {0,1}` for each hop; the map `β_i ↦ β'_i(β_i, l_i) := predict(bayes_update(β_i,
l_i))` is nondecreasing in `β_i` (§2's proof above), and the branch probability
`p_i(l_i|β_i)` is such that the conditional law of `β'_i` given `β_i` is stochastically
nondecreasing in `β_i` in the first-order/MLR sense (higher `β_i` shifts mass toward the
branch `l_i=1`, whose `β'_i(β_i,1)` is itself the larger of the two branch outcomes,
since `eps_bad ≥ eps_good`). A standard coupling argument (couple two filters started at
`β ≼ β''` using the monotone coupling implied by this stochastic order, propagate both
coordinates forward, apply `F` monotone to each) then gives: **`E_o[F(β'_o)]` is
nondecreasing in `(β1,β2)`** — this is the standard building block of monotone-POMDP
filtering theory (Lovejoy 1987; Krishnamurthy ch. 2) applied coordinatewise here because
the two hops evolve independently and the branch weights `p1(l1|β1)p2(l2|β2)` factor.

## 3. P2/P3 — the always-warm clamp theorem (fully provable)

The full model's action also picks the next `(p,w)` context, and its observability is
action-dependent — this breaks the plain single-context monotone-POMDP theorem (§4
below explains exactly how). But **constraining the standby to always be WARM**
(`m≡WARM`, `warm_standby.constrained_policy`'s regime) removes both complications at
once: every action becomes observable (`a=B` is live traffic, `a=A` with `m=WARM`
forced is a standby probe), so the update kernel no longer depends on which action led
here, and the state reduces to `(β1,β2,p)` with `p∈{A,B}` only.

Let `content(β,a) = routing_loss(a) + c_warm` and
`base(β,a) = content(β,a) + E_o[h(β'_o, a)]` — note `base` depends on the *action*
`a` alone, not on the current context `p`, since the update kernel is the same
regardless of context here. Then

```
h(β,A) + g = min( base(β,A), base(β,B) + c_switch )
h(β,B) + g = min( base(β,A) + c_switch, base(β,B) )
```

**Lemma (clamp identity).** Let `d(β) = base(β,B) - base(β,A)` and
`Δ(β) = h(β,B) - h(β,A)`. Then `Δ(β) = clamp(d(β), -c_switch, +c_switch)` exactly.

**Proof.** Write `base(β,B) = base(β,A) + d`. Then
`h(β,A)+g = base(β,A) + min(0, d+c_switch)` and
`h(β,B)+g = base(β,A) + min(c_switch, d)`, so
`Δ = min(c_switch,d) - min(0,d+c_switch)`. Checking this against `clamp(d,-c_switch,
c_switch)` at the three regimes `d ≤ -c_switch`, `-c_switch ≤ d ≤ c_switch`, `d ≥
c_switch` confirms equality in each (this is a pure algebraic identity — it needs no
monotonicity of `d`, only the two `min(...)` expressions above). ∎ **Verified**: max
`|Δ - clamp(d,-c_switch,c_switch)|` = 2.5e-16 to 2.8e-16 across scenarios tested
(`switching_curves.verify_clamp_identity`).

**Proposition (P2, discounted criterion).** `d(β) = base(β,B)-base(β,A)` is
nondecreasing in each of `β1,β2`.

**Proof (corrected 2026-07-18 — see erratum below).** Let `h_n` be RVI's `n`-th
iterate and `Δ_n(β) := h_n(β,B) - h_n(β,A)` (the per-iteration constant `g_n` subtracts
identically from both contexts, so it cancels in `Δ_n` and does not need tracking).
Write `base_n(β,a) = content(β,a) + E_o[h_n(β'_o,a)]` and
`d_n(β) = base_n(β,B) - base_n(β,A)`. The **key step, done in the right order**: rather
than showing `h_n(·,A)` and `h_n(·,B)` are *each* monotone and then subtracting (which
does not preserve monotonicity in general — a genuine gap in an earlier draft of this
proof, caught in external review, see erratum), track `Δ_n` itself as a single object.
Because observability is action-independent here, `E_o[h_n(β'_o,B)]` and
`E_o[h_n(β'_o,A)]` are expectations of the *same* random next-belief `β'_o` under the
*same* branch weights — only which slice of `h_n` is read off differs — so by linearity
of expectation over that shared random variable,

```
E_o[h_n(β'_o,B)] - E_o[h_n(β'_o,A)] = E_o[ h_n(β'_o,B) - h_n(β'_o,A) ] = E_o[Δ_n(β'_o)]
```

Hence `d_n(β) = [path_b_loss(β) - cost_a] + E_o[Δ_n(β'_o)]`. Now induct on `Δ_n`
directly: **base case** `Δ_0 = 0` (constant, trivially monotone). **Inductive step**:
if `Δ_n` is monotone, Lemma A (§2) gives `E_o[Δ_n(β'_o)]` monotone; `path_b_loss(β) -
cost_a` is monotone directly (established already); their *sum* `d_n` is monotone (sum
of monotone functions is monotone — unlike a difference, this step is valid without
further conditions). The same algebra as the Lemma above gives
`Δ_{n+1}(β) = clamp(d_n(β), -c_switch, +c_switch)` pointwise at every iterate `n`
(not just at the fixed point), and `clamp(·,-c,c)` is a nondecreasing scalar function,
so composing it with the monotone `d_n` gives `Δ_{n+1}` monotone. Induction closes;
taking `n→∞` (RVI convergence) gives `Δ` monotone, hence `d = [path_b_loss - cost_a] +
E_o[Δ(β'_o)]` monotone as the sum of two monotone terms. ∎

**Erratum.** An earlier version of this proof asserted "`h(·,A)`, `h(·,B)` are each
monotone (by §2), so their difference composed with a common monotone-preserving map
stays monotone" — this step is false in general (the difference of two monotone
functions need not be monotone; e.g. `x` and `x²` are both nondecreasing on `[0,1]` but
`x - x²` is not). The result (`d` monotone) is unaffected — it was independently
confirmed at 0 numerical violations before and after this fix — but the published
justification was wrong until corrected here. Caught by Codex CLI in an external
planning-stage review, 2026-07-18; see this project's convention of recording exactly
which assumption/step broke rather than silently patching it.

**Verified (strengthened)**: beyond checking the converged fixed point, the proof's
actual inductive claim — `Δ_n` monotone *at every RVI iteration*, not just in the
limit — was checked directly by instrumenting the iteration loop: across 233 iterations
to convergence, zero monotonicity violations of `Δ_n` at any iteration, and the
pointwise clamp identity `Δ_{n+1} = clamp(d_n, -c_switch, c_switch)` held to 1.9e-17 at
every iteration (not only at the fixed point) — i.e. the numerics confirm the corrected
proof's actual induction step, not merely its conclusion.

Zero monotonicity violations of `d` at resolution 100, across every
scenario tried (§4).

**Corollary (the two curves).** From `h(β,A)+g = min(base_A, base_B+c_sw)`: leaving A
for B happens iff `d(β) < -c_switch`. From `h(β,B)+g = min(base_A+c_sw, base_B)`:
leaving B for A happens iff `d(β) > c_switch`. Since `d` is monotone in the product
order, `{d ≥ c_switch}` (bail-from-B region) is an upper set, `{d ≤ -c_switch}`
(resume-from-A region) is a lower set, and each region's boundary is (where it exists
as a function) a monotone curve `β2 = φ(β1)`. **Both curves are level sets of the same
scalar field `d`, at `±c_switch` — the hysteresis band is exactly `{|d(β)| <
c_switch}`.** Nesting (resume curve at lower `β` than bail curve, for matching `β2`,
whenever both exist) follows directly from `-c_switch < c_switch` and monotonicity of
`d`. **Verified**: nesting held at every common `β2` tested; asymmetric switch costs
(`c_resume ≠ c_bail`) only change the clamp bounds, not the argument.

**Proposition (P3, average-cost transfer).** The same clamp identity, and hence the
same monotonicity argument, holds at the RVI fixed point for the long-run average-cost
criterion — the derivation above never used discounting, only the two `min(...)`
expressions defining `h`, which are the same in both criteria (RVI is just undiscounted
value iteration with a pinned reference point). The only assumption this adds is RVI's
own convergence (standard for a unichain average-cost MDP; already relied on elsewhere
in this project, e.g. `average_cost_value_iteration_switch`).

## 4. Gap G1 — why the full unconstrained model isn't provable the same way

**Where the argument breaks.** Increasing differences of `Q(β,k)` across the full
model's 4 actions requires comparing continuation values under *different observation
regimes* — e.g. `(A,cold)`'s deterministic predict vs. `(A,warm)`/`(B,·)`'s Bayes-updated
expectation. By Jensen (since `h` is separately concave in each coordinate — see the
caveat in `dmr/beliefgrid2d.py`'s module docstring about the bilinear, not affine,
product-belief embedding), `E_o[h(posterior)] ≤ h(predict)`, so the *sign* of this gap
is known, but its *magnitude* is a genuine value-of-information term: hump-shaped in
`β` (near-zero at `β∈{0,1}` where there is nothing left to learn, largest at
intermediate uncertainty). A hump-shaped term inside a `Q`-difference breaks the
increasing-differences condition Topkis' theorem needs — no amount of TP2/MLR structure
on the transition/observation kernels fixes this, since the obstruction is in the
*action-dependent observability* itself, not in the channel dynamics.

**What this predicts, concretely: the warm/cold (`m`) component of the policy should
not be a threshold in `β` at all — expect a probing *band* near the routing-switch
boundary, dropped at both extremes.** This is not a numerical artifact; it is the
direct signature of the VoI hump.

**Numerical probe** (`switching_curves_demo.py`, scenario `cost_a=0.08, c_warm=0.06,
c_switch_warm=0.01, c_switch_cold=0.5`):
- **The routing decision's `d`-field held perfectly monotone in every *realistic*
  scenario tested** (varying `c_warm` from 0.005 to 0.07, `c_switch_warm/cold` across
  a 100x range, and separately across all 64 points of the adaptivity-value sweep in
  the paper's §8.6) — zero violations, in all 4 `(p,w)` contexts, every time.
- **Update, 2026-07-18 — this is settled, and the answer is that no general theorem
  is possible: a genuine counterexample exists.** Following a second planning-stage
  review (Codex CLI + an independent Fable-model agent), both recommended *trying to
  break* monotonicity via an adversarial parameter search before attempting any
  sufficient-condition proof — a proof attempt built on a false universal claim would
  be wasted effort regardless of technique. A random search over 250 parameter
  combinations (hop rates, loss probabilities, and costs drawn from wide,
  physically-plausible log-uniform ranges — not an adversarially narrow corner) found
  one at trial 90 with **228 monotonicity violations, magnitude 2.6e-3** at grid
  resolution 30. Re-solved at resolutions 30/60/100/150 to rule out a discretization
  artifact: violation *count* grows with resolution (228→984→2712→6080) while
  per-step *magnitude* shrinks (2.6e-3→1.4e-3→0.89e-3→0.60e-3) — but
  `magnitude × resolution` **converges to a constant ≈0.09** across all four
  resolutions (0.078, 0.085, 0.089, 0.090), which is exactly the signature of a real,
  finite-slope non-monotone dip in the *continuous* field being resolved by
  increasingly fine finite differences (a genuine discretization artifact would show
  `magnitude × resolution → 0`, not a nonzero constant). **The routing threshold is
  therefore provably not universal — G1 is false in general, not merely unproven.**
  The counterexample's parameters (`nhop_demo.py`-style `HopParams`, both hops in one
  scenario): hop1 with `p_gb=0.0053, p_bg=0.72` (extremely short ~1.4-step bursts)
  and `eps_bad=0.91` (near-total loss when bad — a sharp, short-lived, high-contrast
  hop), hop2 with `p_gb=0.018, p_bg=0.073` (~13.7-step bursts) and `eps_bad=0.29`
  (much milder); `cost_a=0.14`, `c_warm=0.036`, `c_switch_warm=0.0027`,
  `c_switch_cold=0.096` (a 36x cold/warm switch-cost ratio, not an extreme value by
  this project's standards). Notably `λ1·λ2 ≈ 0.25` here — *smaller* than the paper's
  calibrated scenario's `≈0.42` — so the naive "vacuous only when both hops are
  persistent" intuition from the planning review does not by itself explain where
  the break happens; the more precise mechanism (plausibly hop1's combination of
  extreme loss contrast with very short bursts producing an unusually sharp,
  short-lived value-of-information spike) was not fully isolated and is left as an
  open question. **Given a general theorem is now known to be false, no sufficient-
  condition proof was attempted** — the honest deliverable is exactly the fallback
  both reviews recommended in advance: routing-threshold monotonicity is empirically
  certified for the realistic/calibrated parameter regime this project actually
  uses (every scenario in `switching_curves_demo.py`, `STAGE0_REPORT.md`, and the
  paper's §8.6 sweep — none of which resemble the counterexample's extreme
  short-burst/high-contrast hop1), explicitly **not** claimed to hold universally.
  Reproduce via `adversarial_search_demo.py`.
- **Gating follow-up, 2026-07-18 — does the field's non-monotonicity actually break
  the routing POLICY's threshold structure, or only the underlying `d`-field?** These
  are different claims: `d_field_full_model`'s zero level set *is* the routing
  decision boundary directly (no `±c_switch` offset, unlike the always-warm model), so
  a column/row with more than one zero-crossing of `d` would mean the policy itself
  flips back and forth as `β` varies — not just that `d`'s slope wobbles. Checked via
  `switching_curves.extract_level_curve` (`multi_crossing_columns`) on the trial-90
  counterexample at resolution 150, in both directions (fixed-`β2`/root-over-`β1`,
  and fixed-`β1`/root-over-`β2`, the latter needed because `check_monotone_grid`
  showed the violations concentrated entirely on the `β2` axis — 0 violations on the
  `β1` axis in every context): **zero multi-crossing columns or rows in all 4 `(p,w)`
  contexts.** Re-checked across all 12 of the 250-trial sweep's violating scenarios
  (at the search's native resolution 30, `zero_crossing_sweep_demo.py`) — same result,
  0 policy multi-crossings everywhere, from the mildest (4 field violations) to the
  worst (228). **So at every counterexample found so far, the routing policy remains
  a clean single-threshold curve in each direction even though `d` itself is not
  monotone** — the non-monotone dip changes `d`'s local slope/magnitude but never
  flips which side of zero a fixed slice sits on. This settles Gap G1 as a
  **field-level, not (yet) policy-level, phenomenon** on every case examined; it does
  not prove no policy-breaking counterexample exists (no theorem, and the search only
  covered these 250 draws), but it is the honest empirical answer this project has
  today, and the reason §4's earlier prediction two paragraphs above ("expect a
  probing band … at the routing-switch boundary" — referring to the *warm/cold*
  policy `m`, not the routing policy `a`) should not be read as implying the routing
  decision `a` itself loses its threshold structure. Reproduce via
  `zero_crossing_check_demo.py` (single-witness, both directions, resolution 150) and
  `zero_crossing_sweep_demo.py` (all 12 violators, resolution 30).
- **REVISION, 2026-07-18 (`policy_multicrossing_targeted_search_demo.py` +
  `verify_policy_multicrossing_demo.py`, task A0-2/#63): the claim above ("routing
  policy never loses its threshold structure") is not exactly true — ONE verified
  policy-level multi-crossing exists, though a repeat search did not reproduce it.**
  The zero-crossing checks above only ever tried the 12+15 witnesses a blind random
  search happened to produce; none of those witnesses' dips sit close to the `d=0`
  boundary (all found dips are `|d|≈0.5–0.95` away from the threshold — see the
  Localization entry below), so a clean single-crossing was never actually
  stress-tested. This follow-up used gradient-free local optimization (Nelder-Mead) to
  directly search for parameters where a known dip's `d`-value is driven toward zero,
  seeded from the 5 largest known field-violators. **First run** (later found to have an
  under-constrained parameterization — see below): 4 of 5 optimizations pushed `|d|` at
  the dip down to the 1e-4–1e-3 range without producing a multi-crossing, but the 5th
  (seeded from trial 24) DID: at context `(p=B, w=cold)`, the column `β2=1.0` (the
  simplex edge — hop2 certainly Bad) showed 2 sign changes close together, converging
  toward `β1≈0.03–0.04` as resolution increased. **Independently confirmed twice** — once
  by this session, once by an independent Codex CLI review that re-ran
  `verify_policy_multicrossing_demo.py` itself rather than trusting the report — as
  present at every resolution tested (30/60/100/150), and confirmed to sit on the exact
  boundary column only (neighboring columns at `β2=0.9467..0.9933` show 0–1 crossings,
  per Codex's check), not a discretization or interpolation artifact
  (`interpolate_batch` does not extrapolate past `β=1`, and the multi-crossing count
  comes from sign changes in the solved grid values directly, not an interpolated
  query). **This one witness is a confirmed, verified counterexample to "the routing
  policy never loses its threshold structure."**

  That same Codex review, however, also caught that `vector_to_params` did not enforce
  `eps_bad>eps_good` or `c_switch_cold≥c_switch_warm` during optimization — nothing
  structurally prevented Nelder-Mead from wandering into a physically-backwards corner
  (though Codex separately confirmed the ONE witness actually found is not itself
  contaminated by this: its `eps_bad>eps_good` and `c_switch_cold≫c_switch_warm` hold
  comfortably, nowhere near either boundary). Fixed by reparameterizing both as a
  positive margin above their partner value. **Re-running the identical 5-seed search
  with the hardened parameterization did NOT reproduce a multi-crossing on any seed** —
  trial 24's optimization pushed `|d|` at the same dip down even further this time
  (1.6e-5, essentially exact zero) yet still produced 0 crossings. This is not a
  contradiction: Nelder-Mead is a local, path-dependent, gradient-free method, and a
  different parameterization changes the search geometry enough to converge to a
  different final point in the 12-dimensional space — "drive `|d|` at one fixed point to
  zero" has many solutions, only some of which happen to also produce a genuine
  multi-crossing at nearby points, and which one a local optimizer lands on is
  sensitive to exactly this kind of reparameterization. **Honest summary: exactly ONE
  independently-verified witness of a policy-level multi-crossing is known; it required
  a specific (lucky) optimization trajectory to find, a repeat search with corrected
  code did not find it or any other instance, and no claim is made about how common or
  rare such witnesses are in the wider parameter space — only that they are not
  impossible, settling the "does the policy ever break" question in the affirmative
  while leaving "how often" open.** #42's paper-text distinction (field- vs.
  policy-level non-monotonicity are different claims) still holds as a logical point,
  but the empirical claim that the policy-level one never happens is now known to be
  false, not just unproven. This motivates task #64 (a possible single-crossing proof)
  with a concrete, sharper question: can single-crossing be proven to hold *away from
  the belief-simplex boundary*, given the one confirmed counterexample lives exactly ON
  that boundary?
- **Task #64 gate check, 2026-07-18 (`policy_multicrossing_interior_search_demo.py`):
  before attempting the (20–80h, per both external reviews) proof itself, a cheap
  directed attack on the refined "away from the boundary" conjecture, per the same
  discipline used throughout this project.** `find_deepest_dip` was modified to
  explicitly exclude any dip within 5% of a belief-simplex edge, forcing it to locate
  the deepest *interior* non-monotonicity instead, then ran the same Nelder-Mead attack
  against all 12 known field-violators (not just the top 5). **Scoped finding (Codex
  review, 2026-07-18, flagged the first draft of this as overclaimed — corrected):
  at resolution 30, using adjacent-grid differences with both endpoints required to
  sit in `[0.05, 0.95]`, only 2 of the 12 known violators have a detectable interior
  dip at all** — for the other 10, every non-monotone violation this specific check
  finds sits within the excluded 5%-of-edge margin at this resolution. This is
  suggestive that Gap G1's non-monotonicity concentrates near extreme beliefs
  (`β` near 0 or 1) at this resolution and grid, on this specific 12-witness sample —
  it does NOT rule out finer-resolution interior violations, a genuinely continuous
  interior counterexample, or a violator elsewhere in the 12-dimensional parameter
  space this sample didn't cover, so it should not be read as a general structural
  fact about the model. Of the 2 seeds with an interior dip, pushing `|d|` down via
  the same optimization (maxiter increased to 600 and convergence status logged,
  after Codex's review noted the original run couldn't distinguish "converged" from
  "merely cut off") **converged cleanly both times** (`scipy.optimize`'s own
  `success=True`, 467–511 iterations) to `|d|=2.5e-6` and `1.3e-6` respectively —
  both roughly an order of magnitude closer to exact zero than even the boundary
  counterexample's `1.6e-5` — yet **still produced zero interior multi-crossings**.

  **Superseded/sharpened, 2026-07-19 (R1/R2, an independent Codex+Fable consultation on
  where to take this question next): the arbitrary "5% of the belief square" boundary
  exclusion above is not the practically-relevant one.** Fable found, and this session
  verified directly (`predict_scalar(beta,hop) = beta*(1-p_bg) + (1-beta)*p_gb` maps ANY
  belief into `[min(p_gb,1-p_bg), max(p_gb,1-p_bg)]` after just one predict step, and
  since every timestep of this belief-MDP applies predict regardless of whether an
  observation occurred that step, this interval is forward-invariant — belief never
  leaves it again once inside), that the ONE known policy-level multi-crossing witness
  (β2=1.0, from the task above) sits at a belief the system can **never actually
  reach**: that witness's own hop2 has `p_gb2=0.0051, p_bg2=0.661`, so its dynamically
  reachable interval is `β2 ∈ [0.0051, 0.339]` — confirmed by iterating `predict_scalar`
  from β2=1.0 itself, which collapses to 0.339 after one step and keeps shrinking
  toward the stationary point. **This is a materially stronger, principled refinement
  than the fixed 5%-margin exclusion**: re-running the reachable-box restriction (not
  a uniform margin) on ALL 12 known field-violators found **0/12 have any d-field dip
  at all inside their own dynamically reachable belief box**, confirmed resolution-
  stable at 30/60/100 (`policy_multicrossing_reachable_search_demo.py`). Since none of
  the 12 seeds qualify as a starting point for a targeted attack, a fresh global
  `scipy.optimize.differential_evolution` existence hunt was run directly over the
  whole physically-plausible 12-D parameter box (not seeded from any known violator),
  maximizing the deepest reachable-box dip depth as its objective — it converged to
  depth=0 at `nfev=240`.

  **CORRECTED, 2026-07-19 (a follow-up Fable-only consultation — Codex was rate-limited
  and unavailable this round, so this correction has only one independent reviewer, not
  two): the depth=0/nfev=240 result above is much weaker evidence than the wording
  implied, and should not be read as "the search came up empty" — it likely never ran a
  real search at all.** Two concrete flaws, both verified directly against the code
  (`policy_multicrossing_reachable_search_demo.py`): (1) `deepest_reachable_dip_depth`
  returns exactly `0.0` when the belief-MDP solve throws ANY exception (lines 247-250),
  silently conflating "solver failed on this parameter combination" with "provably
  monotone" — these are not the same thing, and the conflation biases the objective
  toward 0 for numerically awkward regions rather than reporting them as unknown.
  (2) With `popsize=10` in this 12-D space, `differential_evolution`'s initial
  population is `10*12=120` individuals; `nfev=240` is roughly the initial population
  plus ONE generation — since the true objective is exactly 0 over most of this space
  (even the unrestricted, non-reachable-box violation rate is only ~4.8%, so the
  reachable-box rate is smaller still), the population's fitness spread collapses near
  0 almost immediately, triggering DE's own convergence criterion (population-energy
  std below tolerance) long before anything resembling a real global search happened.
  **This was effectively an unweighted ~240-point random sample of a space where hits
  are already rare even without the reachable-box restriction — near-zero evidential
  weight, not a null result to build on.** The conclusion below is therefore
  provisionally retained on the strength of the 0/12 known-violator check (which does
  not have this flaw) alone, not the global hunt; a corrected, properly-powered search
  (fixed exception handling, a continuous surrogate objective that rewards dragging a
  dip toward the box rather than an all-or-nothing indicator, invariant-biased
  Sobol/multistart sampling, and specifically a **homotopy continuation from each known
  violator** shrinking the relevant hop's `p_bg` to drag its box edge toward the dip's
  location while tracking in-box depth along the path — the cheapest and most directly
  mechanistic check, since it either shows depth collapsing exactly as the box reaches
  the dip, or surfaces a genuine in-box counterexample) is future work, not yet done.

  Combined: the field-level Gap G1 counterexamples found throughout this project may be
  ENTIRELY confined to belief-space regions the system can never occupy in steady
  state (well-supported by the 0/12 check), and the one known policy-level break lives
  there too. The practically-relevant conjecture is therefore refined once more, to
  **"single-crossing holds on the dynamically reachable belief set"** — supported by
  the 0/12 known-violator check, NOT by the (flawed) global existence hunt above, so
  weaker evidence than originally stated but not zero.

  **REFUTED, 2026-07-19 (task R4, the homotopy-continuation check the correction above
  named as future work — done same day): the "single-crossing holds on the dynamically
  reachable belief set" conjecture is FALSE, not just weakly supported.** Per Fable's
  proposed method, starting from trial 23 (one of the 12 known field-violators, whose
  own reachable box has no dip) and continuously shrinking hop2's `p_bg2` from its
  original 0.217 toward 0 (dragging that hop's box upper edge toward 1, the direction
  the original dip sits in) while re-solving the full belief-MDP and re-checking
  in-box depth at every step: **a genuine in-box d-field dip appears partway along the
  path** (first detectable around `p_bg2≈0.042`, i.e. well before the box edge reaches
  1) and, critically, **its location stabilizes at `β2≈0.09–0.10` as resolution
  increases (30→60→100→200→300)** — NOT drifting toward either box edge (`box2=
  [0.0036, 0.9953]` at `p_bg2=0.00466`, so β2≈0.09-0.10 sits comfortably in the
  interior). `depth × resolution` converges cleanly to ≈0.38–0.39 across all 5
  resolutions tested (30/60/100/200/300) — the standard signature this project uses
  throughout for "real dip, not a discretization artifact" (c.f. the original Gap G1
  convergence check above). **This is not merely a field-level (Gap G1) violation:
  checking `extract_level_curve`'s `multi_crossing_columns` directly on this same
  parameter point (`p_bg2=0.00466`, everything else identical to trial 23) finds a
  genuine POLICY-level multi-crossing at `β1≈0.65–0.67`, resolution-stable at 60 and
  100 — and `β1≈0.65-0.67` sits deep inside hop1's own reachable box `[0.0099, 0.8757]`,
  nowhere near either edge.** This is a materially stronger counterexample than the
  original β2=1.0 witness (which lived outside the reachable set): a small (single-
  parameter), physically unremarkable perturbation of an already-known field-violator
  produces a genuine policy-level break WITHIN the belief states the system can
  actually occupy. **The refined conjecture from the correction above is retracted, not
  just weakened.** Reproduce: `policy_multicrossing_homotopy_demo.py` (the continuation
  scan) plus the follow-up policy-level check described above (not yet folded into a
  demo script as of this writing — see the session record for the exact one-off
  verification commands). The honest state of the single-crossing question is now:
  it fails in general (settled, §4 above), it fails even restricted to the belief-
  simplex-boundary-excluded interior (this counterexample), and it fails even
  restricted to the dynamically reachable belief set (this counterexample) — every
  refinement attempted so far has been falsified by a specific witness once looked for
  directly, rather than inferred from the absence of a counterexample in a limited
  search. No sufficient-condition proof attempt is recommended going forward without
  first explaining why THIS mechanism (whatever makes `β2≈0.09-0.10` / `β1≈0.65-0.67`
  special for this parameter family) doesn't recur; that mechanism is not yet
  understood, only its existence is confirmed.

  Given this, the larger proof-attempt investment that the correction above still
  entertained ("worth the larger proof-attempt investment in principle") is now
  actively NOT recommended — the target conjecture it would have aimed at no longer
  holds. Any future proof attempt (single-crossing preservation through the Bellman
  backup's three risk points: the `min` operation, the action-dependent observation
  kernel, and expectation aggregation — see Quah & Strulovici 2012 and Athey 2002, §9)
  would need a still-further-refined, not-yet-identified restriction to even have a
  correct target, and should not be attempted speculatively.
  rather than rushed into an unreliable sketch.
- **Localization, 2026-07-18 (`localize_violations_demo.py`, task D1-2): where is the
  dip, and how far is it from the threshold it never crosses?** `check_monotone_grid`
  alone cannot answer this (counts/max-magnitude only), so
  `switching_curves.localize_monotonicity_violations` was added, returning violation
  masks and the bounding box on the non-differenced axis. At the trial-90 witness,
  resolution 150, **all 4 `(p,w)` contexts show an identical localization**: every
  beta2-axis violation sits in `β1 ∈ [0.62, 1.0]` (58 of 151 rows), with the single
  deepest dip always at `β2≈0.367`. A resolution-independent "dip depth" metric — the
  trapezoidal area between a fixed `β1=1.0` row and its running-max envelope along
  `β2` (this converges, unlike raw violation count or per-step magnitude, because it
  is a genuine Riemann sum for the continuous field's deficit integral; naively
  summing this area over *all* rows instead of a fixed row was tried first and grew
  linearly with resolution — an artifact of more rows being summed, not the field
  changing, caught by the convergence check itself and fixed by fixing the row) —
  confirms convergence: 4.37e-3 → 4.57e-3 → 4.63e-3 → 4.65e-3 across resolutions
  30/60/100/150 (row-mean deficit likewise stable at ≈5.07e-4). Critically, `|d|` at
  the deepest dip is **0.70–0.90 in magnitude** (context-dependent) against a
  switching threshold of exactly 0 — the dip is real, resolution-stable, and
  substantial in depth, but sits nowhere near the decision boundary in this witness,
  which is the quantitative reason D1-1's zero-crossing check above found no
  policy-level break: there is margin, not a coincidence of exact cancellation.
- **Open-region confirmation, 2026-07-18 (`open_region_check_demo.py`, task D1-3):
  is the witness an isolated point, or does the violation persist over a genuine
  neighborhood?** Staged per Codex's review (bisecting all 12 axes at full
  resolution is too expensive to justify up front): stage 1 perturbed each of the
  12 parameters individually by ±{1,2,5,10}% (resolution 30, cheap) — **all 12 axes
  showed nonzero violations at every single perturbation tried**, no exceptions.
  Stage 2 then perturbed **all 12 axes simultaneously** (a strictly stronger
  openness witness than 12 independent 1D lines through the same point, which could
  in principle coexist with the true violating set being a lower-dimensional
  manifold through it) at resolution 100: violations stayed in the 2300–2750 range
  across joint perturbations of ±1%/±2%/±5%, matching the same order of magnitude as
  the unperturbed witness at that resolution (2712, §4 above). This is a genuine
  open 12-dimensional neighborhood, not a measure-zero point or curve — consistent
  with the resolution-scaling argument already given (violation count growing
  roughly like resolution², the signature of a 2D open region being resolved at
  each fixed context, not a single point).
- **Minimal-model confirmation, 2026-07-18 (`always_cold_adversarial_search_demo.py`,
  task D1-5): does the break need the warm/cold choice at all, or just action-
  dependent observability?** Ran the identical adversarial search (same seed,
  same 250-draw log-uniform sampling, minus the two warm-specific costs
  `c_warm`/`c_switch_warm`) against `always_cold_value_iteration`'s routing
  `d`-field — the purest form of the obstruction: 2 routing actions, standby never
  warmed, `a=A` never observes vs. `a=B` always does, no warm/cold layer on top at
  all. **A counterexample exists here too** (trial 114: 155 violations, magnitude
  2.19e-2, resolution 30; **prevalence 15/250, even higher than the full model's
  12/250**), and the resolution-convergence check confirms it is real, not an
  artifact: `magnitude × resolution` converges to ≈0.66 across resolutions
  30/60/100/150 (0.658, 0.657, 0.662, 0.664). **This settles that action-dependent
  observability alone is sufficient to break monotonicity — the warm/cold choice
  is not the source of the obstruction, merely one more setting where it shows up.**
  Practically, this means task D1-4's mechanism hunt (below) can work in this
  simpler 10-parameter, 2-action model instead of the full model's 12-parameter,
  4-action one, without losing the phenomenon being explained.
- **Feature extraction, 2026-07-18 (`invariant_features_demo.py`, task D1-4 phase
  (a)): candidate features, not yet a validated invariant.** Built a new
  solve-derived one-step VoI-gap diagnostic, `J(β)=h(predict(β))-E_o[h(posterior
  then predict(β))]` (a genuinely different quantity from `dmr/voi.py`'s
  `bayes_risk`, which is the exact one-shot Blackwell gap for a *fixed* prior — see
  that module's docstring), reusing the same `predict_scalar`/`obs_prob_scalar`/
  `bayes_update_scalar`/`interpolate_batch` primitives `beliefgrid2d._continuation`
  already combines internally. Extracted per-scenario features across all 250
  trials of the seed=12345 log — per-hop persistence `λᵢ=1-p_gbᵢ-p_bgᵢ`, per-hop
  loss contrast `contrastᵢ=eps_badᵢ-eps_goodᵢ`, and `max_voi_gap` (the VoI-gap field
  maximized over the belief grid and all 4 contexts) — written to
  `output/invariant_features.json`. Descriptive comparison of violator (n=12) vs.
  non-violator (n=238) means: `contrast_product` (ratio 1.86×) and `max_voi_gap`
  (ratio 1.76×) are both notably higher among violators; `λ₂` and `λ_product` are
  notably *lower* (ratios 0.82×, 0.73×) — i.e. the crude "both hops persistent"
  intuition already rejected in the counterexample writeup above is, if anything,
  backwards on average across the full sample: violators skew toward *lower*
  hop-persistence product and *higher* loss-contrast product. None of this is a
  validated invariant yet (n=12 violators, means only, no held-out check) — that is
  #56/#57's job; this is the input feature table for it.
- **Candidate invariant, 2026-07-18 (`invariant_candidates_demo.py`, task D1-4b/#56):
  ranking combinations of #47's features by rank-based AUC (Mann-Whitney U — chosen
  over fitting a classifier since 12 positives is far too few to fit anything beyond
  a simple product/ratio; no scikit-learn in this project's dependencies anyway).**
  `lambda_product` alone is actually *inversely* predictive (AUC 0.307, i.e. oriented
  0.693 the wrong way — confirming #47's finding that violators skew toward *lower*
  hop-persistence product, not higher). `contrast_product` alone reaches 0.795,
  `max_voi_gap` alone 0.757. **The combination `max_voi_gap · contrast_product /
  lambda_product` reaches oriented AUC 0.894** — clearly the best of the six
  candidates tried, and well above any single feature alone. This is a real, if
  informal, signal: high loss-contrast on both hops, combined with a large VoI-gap
  and comparatively fast-mixing (low-persistence) channels, is the profile most
  associated with a monotonicity-breaking scenario in this 250-draw sample. **Not yet
  validated on independent data** (chosen and scored on the same sample) — that is
  #57's job, on a freshly-seeded search.
- **Holdout validation, 2026-07-18 (`holdout_validate_demo.py`, task D1-4c/#57): does
  it generalize, or was 0.894 an artifact of fitting to 12 positives?** Ran an
  independent 250-scenario sweep (same sampling distributions, disjoint seed=99999 —
  9/250 violators this time, vs. 12/250 on the training seed) and scored the same
  `max_voi_gap · contrast_product / lambda_product` candidate without re-fitting
  anything. **Holdout AUC = 0.895, essentially identical to the training-sample AUC
  of 0.894** (single features held up too, in the same rank order: `contrast_product`
  0.787, `max_voi_gap` 0.732, `lambda_product` inversely predictive at oriented
  0.669 — all consistent with the training sample). **This is a validated invariant,
  not an artifact of the small training sample**: high per-hop loss contrast and a
  large solve-derived VoI-gap, scaled inversely by hop-persistence product, is a
  genuine (if informal — this is an AUC-ranked heuristic score, not a proven
  necessary/sufficient condition) predictor of where routing-threshold monotonicity
  breaks in this parameter space.
- **Gate check for a sufficient-condition proof attempt, 2026-07-18
  (`voi_margin_gate_demo.py`, task2-gate/#58): is there room for a Lipschitz/span-
  contraction theorem at the scenario that actually matters, or is #49(b)'s heavier
  derivation not worth attempting?** At the paper's own calibrated scenario
  (`switching_curves_demo.py`'s parameters — zero violations found, resolution 150),
  compared the SCALE of `d_field_full_model`'s own minimum per-step slope (the
  margin available before a violation would appear — 2.9e-4 in the `β1`-direction,
  the tightest of the two) against the SCALE of the VoI-gap field's own slope (this
  script's `voi_gap`, an upper bound on how much the unbounded hump-shaped term
  could plausibly subtract — up to 5.3e-3). **The VoI-gap slope scale is ~18× larger
  than d's own margin (safety-factor ratio ≈0.055) in every one of the 4 contexts.**
  This is a heuristic proxy, not a rigorous base/VoI-term decomposition (no closed
  form for that split exists in the full 4-action model, unlike the always-warm
  sub-model where `base(β,a)` is literally isolable), but the conclusion is
  unambiguous either way: **even at the one scenario the theorem would need to cover,
  the margin is razor-thin relative to the confound term's own scale.** A general
  Lipschitz-bound sufficient-condition proof for §4's Gap G1 is therefore unlikely to
  close cleanly, and #49(b) should go straight to the numerically-certified-policy-
  class fallback rather than investing further effort in the full derivation.
- **Fallback-policy certification, 2026-07-18 (`fallback_policy_certify_demo.py`,
  task #49(c)): given (a) no counterexample witness ever broke the routing policy's
  threshold structure (D1-1/#44) and (b) is ruled out by #58's gate check, certify a
  simple, exactly two-parameter-implementable policy family instead of chasing a
  general theorem.** Family: routing follows the always-warm sub-model's provably-
  optimal hysteresis rule (stay on the current path unless the always-warm `d`-field
  crosses `±c_switch_warm`/`±c_switch_cold`, whichever actually applies given the
  *current* warm/cold state — an earlier draft of this check used `c_switch_warm`
  unconditionally regardless of current state, which is wrong and produced a wildly
  inflated ~330% gap purely from needless extra switching, nothing to do with the
  warm/cold rule; fixed before drawing any conclusion); warm iff `|d(β)| < θ` for a
  single free threshold `θ`. Evaluated exactly (no Monte Carlo) via the existing
  `beliefgrid_warm.evaluate_fixed_policy_belief_grid_warm` against the true Bellman-
  optimal average cost, at both the paper's calibrated scenario and the D1
  counterexample scenario (trial 90):

  | θ | calibrated scenario gap | counterexample scenario gap |
  |---|---|---|
  | 0.005 | +7.91% | +14.55% |
  | 0.01 | +7.91% | +14.77% |
  | 0.02 | +7.91% | +26.10% |
  | 0.05 | +7.91% | +52.03% |
  | 0.1 | +245.75% | +55.87% |

  At small `θ` (comparable to `c_switch_warm=0.01`'s scale), the fallback stays within
  **8–15% of Bellman-optimal in both scenarios — including the one where the general
  monotonicity theorem provably fails.** The gap grows quickly once `θ` is pushed
  too large (warming an overly broad belief region forces near-constant `c_warm`
  payment) — most dramatically at the calibrated scenario's `θ=0.1`, an instance of
  the same over-warming cost the paper's own adaptivity-value sweep (§8.6) already
  characterizes. **Practical conclusion for #49: a simple, provably 2-parameter
  policy — no general sufficient-condition theorem needed — gets within ~10-15% of
  optimal at a small, conservatively-chosen `θ`,** which is the honest, actionable
  deliverable this task asked for once (b) was ruled out.
  wedge's unexplained `β2≈0.2` non-monotone "wobble" (below) tracks the *routing*
  boundary's proximity — was tested directly and did **not** confirm the simplest
  version of that hypothesis: at the natural comparison context `(p=A,w=warm)`, the
  routing boundary only exists for `β2 ∈ [0, 0.127]`, well short of the wobble's
  location at `β2≈0.19–0.44`. Reported as tested-and-inconclusive rather than
  silently dropped; a real explanation would need comparing against a different
  context or a multi-step-ahead notion of "imminent decision," not attempted here.
- **The predicted mechanism was found, but the shape is a wedge, not a thin band —
  a correction to an earlier over-narrow reading.** The first probe (a single
  `β2=0.07` slice, context `(p=A, w=warm)`) showed a 3-point `cold→warm→cold` run
  landing exactly on the routing flip, and was reported as "a narrow probing band
  right at the routing boundary." A full-grid re-analysis (resolution 150, every
  `β2` row, all 4 `(p,w)` contexts, prompted by a request to characterize the
  warm/cold decision precisely enough to act on) shows that slice was
  unrepresentative: the actual warm-region, in every context where `a=A` is the
  routing choice (i.e., whenever this step's decision is "stay on/bail to the
  direct path"), is **contiguous from `β1=0`** at every `β2` row tested (never a
  band floating in the middle) — i.e. it *is* expressible as an upper cutoff
  `β1 ≤ φ(β2)` per row, but `φ` is a wide, `β2`-dependent envelope, not a thin
  strip: `φ(0)≈0.75`, decreasing on average to `φ(0.4)≈0.12`, reaching zero
  (never warm) at `β2≈0.46` — i.e. hop2 suspicion alone, once high enough, kills
  the value of watching hop1 entirely (the same hop2-dominance asymmetry as Stage
  0 Finding 3, showing up again here). `φ` itself is *not* monotone decreasing
  (a real, resolution-stable ~0.15-`β1`-unit widening appears around `β2≈0.2`,
  with no explanation found yet) — consistent with §4's claim that this field has
  no monotonicity proof, unlike the routing `d`-field.
  **Update, 2026-07-18 (`wedge_wobble_decomposition_demo.py`, task #50, time-boxed
  to this single check per the task's own scope): explained.** `Q(A,WARM)-Q(A,COLD)`
  at context `(p=A,w=cold)` splits exactly (reconstruction error 1.9e-16, machine
  precision) into `c_warm − voi_gap(h[:,A,WARM]) + predict_only(h[:,A,WARM]-h[:,A,COLD])`
  — an immediate-cost term, the same VoI-hump term §4/Gap G1 already identifies, and a
  continuation term capturing the propagated value of arriving warm vs. cold next
  step. Checking each term's own monotonicity separately: **the continuation term has
  *zero* violations in either direction (perfectly monotone), while the VoI term
  carries 1761 `β1`-violations and 3865 `β2`-violations overall — and, restricted to
  the documented wobble window `β2∈[0.15,0.30]`, carries *all 622* of the
  `β2`-direction violations there, the continuation term none.** The `φ` wobble is
  therefore the same Gap G1 VoI-hump mechanism reappearing in a different field, not
  a separate phenomenon — closed, not chased further per this task's time-box.
- **Two structurally distinct motives were disentangled, splitting by which routing
  action `a` is chosen this step (this decomposition was missing from the original
  probe).** Within the `a=A` sub-region (routing this step is direct/A): `m=WARM`
  means *probing the relay* — real information-gathering, since the relay's hop1/hop2
  state is the only hidden state in the model — and follows the wedge above,
  identically across all 4 starting `(p,w)` contexts (i.e. independent of whether you
  arrived here already warm or already cold). Within the `a=B` sub-region (routing
  this step is the relay): `m=WARM` means *keeping the direct path A ready as a cheap
  future failover* — since path A carries no hidden state in this model, this has
  nothing to do with information at all, and is a pure switch-cost hedge. It shows a
  much broader, cruder pattern (in the scenario tested, ~76% of the `a=B` sub-region
  warm when arriving from a cold standby, spanning nearly the full `β1×β2` square for
  `β2` roughly in `[0.2, 0.8]`) and, in this scenario, drops to 0% when arriving
  already warm (`(p=B, w=warm)`'s `a=B` sub-region never re-pays to sustain it) — a
  genuine, not-yet-explained subtlety in the timing of the insurance decision that a
  single-slice probe could not have revealed. The routing decision itself has exactly
  1 transition (a clean threshold) on every slice checked, in every context — the
  asymmetry between "routing is clean, warm/cold is not" is the real finding here, not
  the specific shape of any one slice.
- **The 2D interaction appears ESSENTIAL to the counterexample — no violation found in
  the 1D single-hop reduction, 2026-07-18 (`oned_always_cold_demo.py`, task A0-1/#62,
  following up on a second Codex+Fable consultation about next directions after task
  #49; corrected after an independent Codex review of this specific task's
  implementation caught a stale-`d`-recomputation bug and a sampling-distribution
  mismatch versus the 2D search — both fixed, see the script's docstring/comments for
  the erratum).** The always-cold sub-model was reduced to a single relay hop (1D
  belief `b=P(hop=Bad)`, no second hop, no warm/cold layer — the same action-dependent-
  observability obstruction in its most minimal possible form) via a self-contained
  local RVI (`np.interp`, no new grid class, per Codex's suggested cheapest
  implementation). A 250-scenario adversarial search, with sampling ranges matched
  exactly to `always_cold_adversarial_search_demo.py`'s (not merely "similar style" —
  an earlier draft used narrower ranges, caught and fixed), found **zero monotonicity
  violations in every scenario, and none of the 10 closest-to-violating trials (by
  smallest positive per-step margin) flip to violating when re-solved at 4x
  resolution** either. This is empirical evidence from 260 solves at finite resolution,
  not a proof of a continuous-domain negative — but it is a reasonably thorough one, and
  is consistent with the obstruction found in §4's 2-hop counterexamples not being
  merely "action-dependent observability, applied once" but specifically requiring the
  **joint (β1,β2) structure of a single relay path composed of two hops**, where path
  B's loss probability and continuation value are a genuinely 2D function of both
  hops' beliefs at once, not two independent 1D problems. This is consistent with
  (though not derived from) Meshram, Manjunath & Gopalan (2018)'s threshold-structure
  results for a similar single-arm "observe-while-active" restless-bandit setting.
  **Practical implication for task #64 (a possible single-crossing proof)**: since even
  this thorough a 1D search of the *same* observability mechanism found nothing, the 2D
  case's difficulty looks genuinely specific to the joint structure — a proof attempt
  should target exactly where the joint-dependence enters the Bellman backup (the
  composite loss probability / the shared continuation value), not the observability
  mechanism in isolation, which this 1D result suggests is not by itself the culprit.
- **Task #65 (A-fallback), 2026-07-18 (`calibrated_box_certificate_demo.py`): an
  attempted certificate for the calibrated parameter box — INVALIDATED on its own
  terms by an independent Codex review, corrected to an honest (much weaker)
  diagnostic.** First attempt: an empirical curvature magnitude (max observed second
  finite difference at resolution 300) plugged into the textbook quadratic-
  interpolation-error bound `M2·h²/8`, compared against the observed per-step
  difference at coarser resolutions, framed as "certified" wherever the difference
  exceeded that bound. **A dedicated Codex CLI review of this specific script found the
  argument was not just imprecisely worded but mathematically wrong**: (1) `M2·h²/8`
  bounds how far a linear interpolant's *value* can be from the true function — the
  right quantity for a *monotonicity* claim is a bound on the *derivative*, a different
  (looser) inequality; (2) the script's `min_slope` was actually an undivided grid-step
  difference, not a slope, mislabeled throughout; (3) the check only examines second
  differences along grid lines, saying nothing about off-grid-line points or mixed
  partials; (4) most fundamentally, `d_field_full_model` is built from `min()` over
  action subsets (see that function's own docstring) and can have genuine kinks at
  action-switching beliefs, where a uniform second-derivative bound may not exist at
  all — and the resolution-300 "fine" grid used to estimate it is only 2× resolution
  150's spacing, nowhere near fine enough to trust as an order-of-magnitude-better
  reference even setting the kink issue aside. **Corrected conclusion: this is a loose
  empirical magnitude comparison, not a certificate or a proof of any kind** — the raw
  grid-step differences do comfortably exceed the (unsound) naive error term at
  resolutions 60 and above, which is mildly reassuring but adds no rigor beyond the
  zero-violation results `check_monotone_grid` already establishes directly elsewhere in
  this project. A genuine interval-arithmetic or validated-numerics treatment — one that
  handles the `min()`-induced kinks explicitly rather than assuming smoothness — remains
  open future work, not completed here. This is recorded as a worked example of a
  plausible-sounding proof technique that turned out not to fit the problem, caught by
  review before it was trusted, not as a validated result.

## 5. Gap G2 — the price of probing freedom

Comparing the always-warm model's theorem-backed curves against the full
unconstrained model's curves (same scenario, context `w=warm` in the full model for a
fair comparison against the always-warm sub-model's assumption), at the 12 `β2` values
where both curves exist: the full model's bail curve sits at a **higher `β1`** than
the always-warm curve at every matching `β2` (mean shift +0.29, max +0.36, in this
scenario's `c_warm=0.06` regime — the shift was much smaller, ~0.005, at `c_warm=0.01`,
since forcing "always warm" is a far more expensive commitment at higher `c_warm`).
Interpretation: having the *option* to let the standby go cold later (saving `c_warm`)
makes staying on path B more attractive now, since bailing to A no longer locks you
into paying `c_warm` forever — the full model tolerates worse `β1` on path B before
bailing. This quantifies "the price of probing freedom" the always-warm model pays for
its provable structure.

## 6. The n-hop generalization (`dmr/nhop.py`, 2026-07-18)

Everything in §1 and §3 is stated for exactly 2 hops. Both results generalize to an
n-hop serially-composed relay path (single relay path, still a *binary* routing choice
between the direct path A and the n-hop relay B) — verified here for n=3, alongside two
places the generalization genuinely stops.

**Proposition 1' (n-hop factorization).** At full independence across all n hops
("`rho=0`" for n hops), `T = T_1 ⊗ ... ⊗ T_n` (Kronecker product) and the decomposed
likelihood factors n-ways, `P(l_1,...,l_n|s_1,...,s_n) = Π_i P(l_i|s_i)`, so a product
prior `b_1 ⊗ ... ⊗ b_n` stays a product under predict and decomposed-observation Bayes
update, by the identical argument as Proposition 1 (§1), applied inductively over
factors. **Verified** (`nhop.verify_factorization_nhop`): for n=3, max deviation from
product form over 10 random predict/update steps was 3.6e-16. The n=2 case of this
module's `joint_transition_matrix_nhop`/`decomposed_obs_likelihood_nhop` was checked to
match `channels.py`'s existing (independently-implemented) versions exactly
(`max diff = 0.0`) before trusting the n=3 result.

*Scoping note on the correlation knob*: the comonotone-copula construction
(`channels.py`'s `_comonotone_coupling`) generalizes to n hops via `min(u_1,...,u_n)`
(still a valid copula, still exactly marginal-preserving for every mixing weight), but
this single knob cannot express *heterogeneous* pairwise correlation (e.g. hops 1-2
correlated, hop 3 independent) — only a single global correlation level across all
hops. If heterogeneous structure is ever needed, the honest model is a hidden
common-environment modulator (flagged as the "more physically honest" alternative in the
original 2026-07-17 formalization review, never built), not an extension of this
one-parameter family.

**Proposition 2'/3' (n-hop clamp theorem).** For a **binary** routing choice (path A vs.
the n-hop relay B) with the standby forced always-WARM, the derivation in §3 goes
through unchanged with `i` ranging over `1..n`: `path_b_loss(β) = 1 - Π_i(1-e_i(β_i))`
is nondecreasing in every `β_i`; Lemma A (§2) applies per-coordinate since the n hops
evolve independently and the branch weights `Π_i p_i(l_i|β_i)` factor; the clamp
identity's algebra never refers to dimension. So the same induction (now on n
coordinates) gives `d(β_1,...,β_n)` monotone in the product order on `[0,1]^n`, and the
two switching curves become two nested level *hypersurfaces* of the same scalar field
`d`, with the identical exact hysteresis band `{|d(β)| < c_switch}`. **Verified**
(`nhop.always_warm_value_iteration_nhop`, n=3, resolution 20, 9261 grid points): `d`
monotone along all 3 axes with **zero violations**, and the clamp identity held to
2.8e-17.

This binary-routing-choice generalization is more general than "more hops": it also
covers the case where *both* routing alternatives are hop-chains with their own hidden
state (e.g. two different-length candidate relay routes, always-warm on both) — the
clamp/monotonicity argument only needs a scalar `d` comparing two `base(β,a)` terms
under a shared, action-independent observation kernel, and doesn't care whether `β` is
2-, n-, or `n_A+n_B`-dimensional.

**Where the generalization stops: 3+ routing *alternatives* (not 3+ hops).** The clamp
identity is fundamentally a two-context phenomenon — `min` over 3+ candidate routes
(e.g. choosing among 3+ candidate relay vehicles) does not reduce to a single scalar
`d`; the pairwise fields `d_pq` between any two alternatives depend on the third
alternative's belief in general, and the upper/lower-set argument for a single
monotone threshold curve has no direct analogue. This is restless-bandit territory
instead, and switching costs are known to generally *break* Whittle indexability there
(Jun, 2004 — a widely cited survey establishing this negative result for bandits with
switching costs; Glazebrook, Ruiz-Hernandez & Kirkbride, 2006, for special indexable
sub-families) — combined with the observe-while-active structure already cited
(Meshram, Manjunath & Gopalan, 2018), the M-relay-vehicle case is "per-arm index +
hysteresis heuristic, no exact structure expected," a different research program from a
direct extension of this section's theorem, not attempted here.

**A modeling assumption this exercise surfaced (not previously stated explicitly).**
The warm action modeled throughout this project (n=2 and the n-hop generalization
alike) is all-or-nothing: choosing to probe means observing every hop's loss bit at
once. Physically, a relay node partway along the chain (e.g. the car, at hop 2 of a
drone→car→WAN chain) could probe *its own* onward hop without the upstream hop's
participation — per-segment probing is not modeled here, and allowing it would likely
reshape the warm/cold wedge (§4/§8.3 of the paper) since hop 2 already dominates the
decision. This is recorded as an explicit scoping choice rather than an oversight,
because it is exactly the class of silent, physically-motivated assumption external
review has caught twice before in this project (the original action-independent-
observation bug, and the "narrow band" mischaracterization corrected in the paper's
§8.3) — better to name it than to let it hide.

## 7. rho-perturbation theory: exact (β1,β2,κ) chart and an O(rho²) robustness theorem

§1's factorization is `rho=0`-only. This section asks what happens for small `rho>0`,
using standard MDP perturbation-analysis and filtering-theory techniques rather than
falling back on the general 4-state simplex solve, prompted by a second planning-stage
review (Codex CLI + an independent Fable-model agent, 2026-07-18).

### 7.1 An exact global chart of the belief simplex

**Claim.** `(β1, β2, κ)` with `κ := b_BB - β1·β2` (the covariance between the two hops'
Bad-indicators under the current belief) is an exact, invertible chart of the 4-state
belief simplex:

```
b_GG = (1-β1)(1-β2) + κ,  b_GB = β2(1-β1) - κ,  b_BG = β1(1-β2) - κ,  b_BB = β1β2 + κ
```

(direct algebra from `β1 = b_BG+b_BB`, `β2 = b_GB+b_BB`, `κ = b_BB - β1β2`; the simplex
constraint `Σb=1` is preserved automatically). `κ=0` is exactly the product-belief
submanifold of §1; `κ` is bounded by the Fréchet–Hoeffding limits
`max(-β1β2,-(1-β1)(1-β2)) ≤ κ ≤ min(β1(1-β2),β2(1-β1))`, with the comonotone coupling of
`channels.py` sitting at the upper bound.

**Proposition (exact predict-step recursion).** Let `λ_i := 1-p_gb,i-p_bg,i` (hop `i`'s
second eigenvalue, already used in §2). For the mixture transition
`T(rho) = (1-rho)·T_indep + rho·T_comon` (`channels.py`'s construction), the predict
step is *exactly* (not just to first order in `rho`):

```
β1' = predict1(β1),  β2' = predict2(β2)          (rho-independent, as established in channels.py)
κ'  = κ·[(1-rho)λ1λ2 + rho·M1] + rho·(M0(β1,β2) - β1'β2')
```

where `M0(β1,β2)` and `M1` are constants of the comonotone coupling's
`min(q1(s1),q2(s2))` values at the 4 joint states (`q_i(s_i) := P(s_i'=Bad|s_i)`); `M0`
is the `κ=0` baseline of `E[min(q1,q2)]` and `M1` is its (κ-)linear coefficient. **Proof**:
direct computation of `E[s1'·s2']` by expanding `q_i(s_i) = p_gb,i + λ_i·s_i` (writing
`s_i∈{0,1}` as its own indicator) and using `E[s1 s2] = β1β2+κ` — the algebra is
mechanical but not reproduced here; see `nhop.py`'s docstring conventions for the
matching index scheme. **Verified**: matches `b @ joint_transition_matrix(hop1,hop2,rho)`
to `2.2e-16` over 2000 random `(β1,β2,κ,rho)` trials spanning the full valid range,
including `rho∈[0,1]` (i.e. this is an *exact* identity, not a perturbative
approximation — the perturbative content is entirely in choosing to treat `rho` as
small, not in the formula itself). At `rho=0`: `κ' = λ1λ2·κ` exactly — a clean
contraction whenever `|λ1λ2|<1` (always true for a genuine 2-state chain), structurally
identical to the geometric contraction of projection-filter error in factored dynamic
Bayes nets (Boyen & Koller, 1998).

**Bayes-update effect on κ (first order).** A parallel expansion of the decomposed-
observation Bayes update gives, to first order in `κ` (writing `e_iB := p_i(l_i|Bad)`,
`γ_i` the loss-vs-no-loss likelihood gap, `Z_i` the standard per-hop marginal
observation probability already computed by `beliefgrid2d.obs_prob_scalar`):

```
β1_post ≈ β1_post^(standard) + (κ_pred·γ2/(Z1 Z2))·[e1B - β1_post^(standard)·γ1]
```

where `β1_post^(standard)` is exactly what `bayes_update_scalar` already computes
(confirmed by direct substitution: the `κ=0` leading term reduces exactly to the
existing per-hop Bayes formula). **Verified**: over 500 random trials with modest `κ`
(30% of its Fréchet–Hoeffding range), the first-order-corrected formula's mean absolute
error was `1.4e-3` versus `2.3e-2` for the existing `κ=0` formula applied naively to a
`κ≠0` prior — a 16.7x reduction, confirming the correction's sign and magnitude, with
the residual consistent with the expected `O(κ²)` truncation error of a first-order
expansion.

### 7.2 The O(rho²) robustness theorem

**Setup.** Let `π*` be the (rho=0)-optimal policy — exactly what `beliefgrid2d.py`
already computes, a fixed function of `(β1,β2,context)` only (it never looks at `κ`,
since at `rho=0` `κ≡0` along any trajectory it could ever encounter). Define
`g(π*,rho)` as `π*`'s long-run average cost when *deployed* in the true environment at
correlation `rho` (a well-defined quantity for any `rho`, evaluable via
`beliefgrid_warm.evaluate_fixed_policy_belief_grid_warm`, added for this purpose — a
policy-*evaluation* variant of the existing belief-simplex-grid solver that plugs in a
fixed action per grid point instead of minimizing over actions). Let `g*(rho)` be the
true optimal average cost at correlation `rho` (`beliefgrid_warm.
belief_grid_value_iteration_warm`, already existing and validated for general `rho`).

**Proposition (first-order robustness).** `g(π*,rho) - g*(rho) = O(rho²)` near `rho=0`.

**Proof.** For a *fixed* policy, the average-cost derivative w.r.t. a smooth parameter
of the transition kernel is the standard Markov-chain performance-sensitivity formula
(Schweitzer, 1968; developed into a full sensitivity calculus — "performance
potentials" — by Cao & Chen, 1997): `dg(π*,rho)/drho|_0 = Σ_x μ(x) Σ_x' (dP/drho)(x'|x)
h_π*(x)`, where `μ` is `π*`'s own stationary belief distribution at `rho=0` and `h_π*`
is exactly the relative value function RVI already computes for `π*` — the "potential"
in Cao's terminology is this project's `h`. Since `T(rho)` is linear in `rho` by
construction (`channels.py`), `dP/drho` is known in closed form (§7.1's exact
recursion). This derivative is well-defined and finite. Separately, `g*(rho)` is the
optimal value of a parametrized family of MDPs in which `π*` is optimal exactly at
`rho=0`; by the envelope theorem for parametrized optimization (Milgrom & Segal, 2002),
`g*(rho)` is differentiable at `rho=0` with *the same* derivative as `g(π*,rho)` there
(re-optimizing the policy as `rho` moves away from 0 is a second-order effect at the
point where `π*` is exactly optimal). Hence `g(π*,·)` and `g*(·)` agree in *both* value
and first derivative at `rho=0`, so their difference is `O(rho²)`. ∎

**Caveats (stated explicitly, not hand-waved).** (i) This is an *expectation-level*/
averaged statement, not a pointwise-in-time one: the per-step Bayes update can
transiently *expand* `κ` (the update-derivative can exceed 1 for a surprising
observation against a confident belief) even though predict contracts it in
expectation (§7.1); the rigorous version needs an averaged/mixing argument in the style
of Boyen & Koller (1998), not naive per-step composition. (ii) The envelope-theorem
step requires the optimal action to be unique (a.e. under `π*`'s stationary belief
measure) — if that measure puts mass exactly on an indifference curve (the switching
curves of §3), the derivative can have a kink rather than being smooth; not expected
generically, not proven absent here. (iii) Full rigor for differentiability of a
stationary functional of a continuous-state filter is a genuinely deep question (Han &
Marcus, 2006, needed substantial work to prove analyticity even for *uncontrolled* HMM
entropy rate) — this project's established posture applies: state the derivative
formally via the standard fixed-ingredient formula above, verify by direct
finite-difference-style computation, and do not claim more rigor than that.

**Numerical check.** Using the new noise-free exact evaluator (no Monte Carlo — grid
resolution 12, `rho ∈ {0, 0.01, 0.02, 0.04}`, same scenario as §3/§4): the raw gap
`g(π*,rho)-g*(rho)` was `4.7e-5, 4.7e-5, 4.8e-5, 4.8e-5` — i.e. **flat to within
~1e-6 across the entire tested range**. That residual ~1e-6 excess sits at or below
this grid resolution's own discretization/interpolation noise floor (the nonzero
`4.7e-5` baseline *at rho=0*, where `g(π*,0)` and `g*(0)` are solving the literal same
problem and should agree exactly, is itself such an artifact — comparing a policy
solved on the 2D `(β1,β2)` grid against its evaluation on the 4-state simplex grid at a
different resolution/geometry). For scale: `g*(rho)` itself changes by `-1.08e-4` over
this same range (slope `≈ -0.0027` per unit `rho`) — **about 100x larger** than the
observed excess-gap growth. This is consistent with a vanishing (or at least strongly
suppressed) leading-order term, as the theorem predicts, but grid resolution 12 is not
fine enough to distinguish "exactly quadratic" from "some other rapidly-vanishing
higher-order behavior" — reported honestly as suggestive, not a precision confirmation
of the exponent. An earlier attempt at this check using Monte Carlo rollout evaluation
(500-2000 trajectories) was **underpowered**: its standard error (~3e-4) exceeded the
very effect being measured (~1e-4), and is not reported as evidence either way — the
exact grid evaluator above replaced it specifically because of this.

## 8. Why hop decomposition is decision-theoretically nicer than composite

Four points, from weakest to strongest:

1. **Closed form, not "generic parameters."** Entanglement under composite
   observation is forced iff `(e1_bad-e1_good)(e2_bad-e2_good) ≠ 0` — exactly when hop
   identity carries any decision-relevant information at all. There is no regime where
   hop identity matters for decisions but composite observation stays separable.
2. **The timing asymmetry.** Composite's no-loss branch (`o=0`) *is* separable; only
   loss events entangle. The composite controller's belief is product-form exactly
   when nothing is happening, and becomes a genuinely joint, *negatively correlated*
   ("explaining away": "one of the hops is bad but I can't say which") posterior
   precisely at the moments a bail/ride-out decision must be made. Decomposition
   removes exactly the ambiguity that appears exactly when it would matter.
3. **The geometric payoff.** Under decomposition (rho=0), the entire warm-standby
   policy is characterized by two monotone curves on the unit square, provably so in
   the always-warm sub-model (§3) and numerically confirmed in the full model (§4).
   The composite controller's reachable beliefs live on a genuinely 3-dimensional
   subset of the simplex with no comparable low-dimensional characterization — there
   is no 2-curve picture to draw for it, at any switching-cost regime.
4. **The engineering consequence** (direct implication for Stage 1's car-side control
   channel, `PLAN.md` §4.1): a product sufficient statistic means each hop's filter
   can run exactly where its measurements originate — the car maintains `β2`, the
   drone maintains `β1` — and fusing them for a routing decision is mere
   concatenation, with zero information loss at `rho=0`. Composite observation admits
   no such decentralization; fusing a single ambiguous bit still requires the full
   joint belief machinery regardless of where the bit was measured.

## 9. Literature

- Lovejoy, W.S. (1987), and Krishnamurthy, V., *Partially Observed Markov Decision
  Processes* — MLR/TP2 monotone-filter and monotone-value-function lemmas underlying
  §2/§3's inductions.
- Ly Vath, V. & Pham, H. (2007) — continuous-time optimal switching / entry-exit
  problems; the two-curve hysteresis structure's continuous-time analog. The discrete
  clamp identity in §3 replaces their viscosity-solution machinery entirely for this
  finite-state case — cited as the nearest continuous-time picture, not as directly
  covering this setting.
- Banks, J.S. & Sundaram, R.K. (1994); Asawa, M. & Teneketzis, D. (1996) — bandits
  with switching costs, the discrete-time context for §3's structure.
- Krishnamurthy, V. & Djonin, D. (2007), IEEE Trans. Signal Processing — structured
  threshold policies for dynamic sensor scheduling with action-dependent observation
  kernels; a different cost/stopping structure than here, cited as the nearest
  neighbor for "threshold policy despite controlled observability," not as covering
  this exact model.
- Meshram, R., Manjunath, D. & Gopalan, A. (2018), IEEE Trans. Automatic Control —
  Whittle index for restless hidden-Markov bandits, "observe only while playing" with
  belief drift while resting; single-arm, 1D belief, no switching cost. The closest
  prior result to this project's "observe-while-active" structure, but not the 2D
  switching-cost case here.
- Liu, K. & Zhao, Q. (2010), IEEE Trans. Information Theory; Ahmad, S. et al. (2009)
  — Whittle index / myopic-sensing optimality for GE channels, already cited in
  `FORMALIZATION_REVIEW.md` §3 as covering Stage 0's Finding 2 (no-switching-cost
  case) as a known corollary, not this note's switching-cost case.
- Jun, T. (2004) — survey establishing that switching costs generally break Whittle
  indexability in restless bandits; Glazebrook, K.D., Ruiz-Hernandez, D. & Kirkbride,
  C. (2006) — indexable special-case families with switching costs. Both cited in §6
  to scope exactly where this project's clamp theorem stops applying (3+ routing
  alternatives / candidate relay vehicles), not as results this project builds on.
- Schweitzer, P.J. (1968), J. Appl. Probability — the Markov-chain average-cost
  parameter-derivative formula underlying §7.2's envelope-theorem argument.
- Cao, X.-R. & Chen, H.-F. (1997), IEEE Trans. Automatic Control; Cao, X.-R. (2007),
  *Stochastic Learning and Optimization* — "performance potentials"/perturbation
  realization, developing Schweitzer's formula into a full sensitivity calculus; the
  "potential" is exactly this project's RVI relative value `h`.
- Milgrom, P. & Segal, I. (2002), Econometrica — envelope theorem for arbitrary choice
  sets, the standard tool transferring a fixed policy's parameter-derivative to the
  optimal value's derivative in §7.2.
- Boyen, X. & Koller, D. (1998), UAI — factored-filter projection-error analysis with
  geometric contraction in factored dynamic Bayes nets; the closest structural
  precedent for §7.1's `κ` contraction and the uniform-in-time caveat in §7.2.
- Han, G. & Marcus, B. (2006), IEEE Trans. Information Theory — analyticity of HMM
  entropy rate; cited in §7.2 as the level of genuine difficulty full rigor for filter-
  functional differentiability requires, to calibrate how much this project claims
  (the formal derivative + numerical check, not a fully rigorous differentiability
  proof).

### 9.1 Novelty check for §11's three main claims (2026-07-19, web literature search)

Before treating §11's results as publication-worthy, ran a targeted web literature search
against the three specific claims (the `Phi<=c_warm` bound, the `pi_b>1/2` interior-optimum
non-monotonicity, and the pure-Gilbert POMDP-to-finite-MDP reduction). Honest verdict for each,
with the closest prior work found — **none of the three exact results were found already
published, but in each case the underlying PROOF TECHNIQUE or MECHANISM is standard/classical**,
so the contribution (if pursued toward publication) should be framed as a novel closed-form
application/synthesis to a specific, well-motivated problem, not as a fundamentally new
mathematical method. Search was web/arXiv-based only — paywalled journals and niche
networking-systems venues (SIGCOMM/NSDI/INFOCOM) were not exhaustively covered.

- **`Phi<=c_warm` domination bound (§11.3)**: the proof technique (replay the other regime's
  optimal policy as a feasible candidate, i.e. a "genie-aided"/coupling bound) is standard
  practice in the POMDP/restless-bandit literature (e.g. Krishnamurthy's "Structural Results for
  Partially Observed MDPs," arXiv:1512.03873, and related myopic-policy-bound papers), but no
  source was found stating this specific asymmetric-switching-cost inequality. **Closest prior
  work, worth reading/citing directly before any publication attempt**: Wang, J., Nazarathy, Y.
  & Taimre, T. (2021), "The Value of Information and Efficient Switching in Channel Selection,"
  *Probability in the Engineering and Informational Sciences* (arXiv:2101.03888) — studies
  exactly the "full observation of all channels vs. partial observation of only the active one,
  with a switching cost" setup (structurally identical to warm-vs-cold here), but derives EXACT
  closed forms for each regime separately rather than a coupling/replay domination inequality
  between them.
- **`pi_b>1/2` interior-optimum non-monotonicity (§11.2)**: no source found establishing this
  exact mechanism (closed-form belief-based interior maximum in `P(GG|return)`, propagating to a
  provably multimodal average-cost objective with a derived global-detachment threshold) for a
  2-state hidden-Markov "when to return" problem. Two notable adjacent findings: (a) classical
  equipment-replacement literature on non-monotone hazard rates (OR Spectrum-era results) shows
  the conceptually right SHAPE of claim ("non-monotone underlying rate implies the optimal
  average-cost policy need not be a simple threshold rule") but in an unrelated hazard-rate
  replacement setting, not a hidden-Markov belief-of-return calculation; (b) recent work
  explicitly aims to PROVE monotonicity for structurally similar Gilbert-Elliott-type scheduling
  problems (e.g. "Structural Monotonicity in Transmission Scheduling for Remote State Estimation
  with Hidden Channel Mode," arXiv:2601.19131), suggesting the field's default expectation is
  monotonicity — which makes a genuine, closed-form-characterized non-monotonic counterexample
  more notable than redundant, if it holds up to closer scrutiny.
- **Pure-Gilbert POMDP-to-finite-MDP reduction, combined into closed-form switching boundaries
  (§11.1)**: the underlying MECHANISM (belief collapsing to a finite set of simplex corners when
  observations deterministically reveal the hidden state, reducing a belief-POMDP to an exactly
  solvable finite MDP) is classical/textbook in AI-planning POMDP theory ("deterministic POMDP").
  Gilbert-Elliott-channel opportunistic-access literature (Ahmad, Liu, Javidi, Zhao,
  Krishnamachari (2009), IEEE Trans. Info. Theory, arXiv:0811.0637; Zhao, Krishnamachari, Liu
  (2008), IEEE Trans. Wireless Comm.) uses the same channel-model family and the same
  deterministic-sensing-reveals-state mechanism, but toward myopic-sensing-optimality/Whittle-
  index questions, not a warm-vs-cold monitoring-cost boundary. No source combined (i) the exact
  finite-MDP always-warm reduction, (ii) the exact semi-Markov always-cold reduction, and (iii) a
  symbolically-derived closed-form boundary equation between the two — this specific three-part
  package appears to be original packaging of established sub-techniques, not a rediscovery.

### 9.2 Follow-up novelty check: is `pi_b>1/2` itself a known dividing line? (2026-07-19)

§9.1's `pi_b>1/2` entry above was written after only checking the specific TWO-COMPONENT
conjunction claim against restless-bandit/replacement/scheduling literature. A second, more
targeted round asked the abstract question directly: is "a 2-state Markov chain's aging/staleness
behavior is monotone or non-monotone in elapsed time, exactly according to whether the chain's own
stationary probability of the unfavorable state exceeds 1/2" already known as a general fact,
independent of this project's specific two-component/conjunction framing? Searched three adjacent
angles: distributed sensing/networking, real-options/economics timing theory, and pure
probability/renewal/reliability theory.

**This search materially changes the novelty assessment, and the result should be read as a
correction, not a footnote.** A very close SINGLE-COMPONENT precedent was found: **"On the
Monotonicity of Information Aging" (arXiv:2403.03380, 2024)** studies estimation error (via
conditional entropy) as a function of Age-of-Information for a single Markov-chain-modulated
state, and its monotonicity-vs-non-monotonicity dividing line is **governed by exactly the same
condition** — whether the chain's stationary probability of the unfavorable state exceeds `1/2`.
This is the identical threshold, for a closely related (single-source aging/estimation-error)
question, published one and a half years before this project's finding. No two-component
CONJUNCTION analog of it was found anywhere (real options/economics timing theory and pure
probability/renewal/reliability theory searches came back empty on the two-factor product
structure specifically), and this project's contribution — propagating the belief-level
non-monotonicity into a genuinely multi-modal AVERAGE-COST DECISION objective, with a derived
closed-form threshold for when the interior optimum becomes globally relevant to the decision (not
just a probabilistic curiosity) — was not found stated anywhere either.

**Revised honest reading**: the CORE dividing-line insight (`pi_b` vs. `1/2` governing
monotonicity of 2-state Markov aging behavior) is not this project's independent discovery — it
already exists, recently, in the Age-of-Information literature, for a structurally adjacent
single-component problem. §11.2's genuine contribution is narrower than first framed: extending
that same mechanism to a two-component AND/conjunction structure, and showing it survives into a
real, closed-form-characterized, decision-relevant multimodality (not just an abstract probability
fact). **arXiv:2403.03380 must be read in full and cited/compared directly before any publication
attempt** — both to credit the closest prior work and because the overlap in the specific
threshold condition (not just the general shape of claim) is close enough that failing to cite it
would read as a serious oversight to any reviewer familiar with the AoI literature.

### 9.3 Novelty check for §11.5's phase-diagram trend (2026-07-19)

Checked whether "the value of continuous monitoring over blind operation grows with the
underlying Markov chain's persistence/autocorrelation, vanishing as the chain approaches i.i.d."
(§11.5's corrected `lambda`-trend) is already established. **Verdict: yes, cite it — do not claim
it as novel.** Both an independent web-search literature agent and `opus-symbolic-advisor`'s own
search converged on the opportunistic-spectrum-access/restless-bandit-over-Gilbert-Elliott
literature as the source: **"Optimal Power Allocation over Two Identical Gilbert-Elliott
Channels" (arXiv:1210.3609)** — a two-Gilbert-Elliott-channel structure directly matching this
project's two-hop setup — and **Zhao, Krishnamachari & Liu, "Myopic Sensing for Multi-Channel
Opportunistic Access" (arXiv:0712.0035)**, which classically formalize how channel persistence
governs the value of sensing/exploitation. **Important caution, confirmed independently by both
searches**: plain Age-of-Information (AoI) literature is NOT the right prior art here — AoI's own
monotonicity runs the OPPOSITE direction (fast-changing sources need MORE frequent refresh) —
unlike §9.2's `pi_b>1/2` finding, where AoI (arXiv:2403.03380) correctly WAS the closest prior
work. General value-of-information-in-MDP framings ("MDPs with observation costs,"
arXiv:2201.07908) and time-correlated-channel remote-estimation scheduling (arXiv:2303.16285,
2403.13898) are also relevant background, cited for the general framing this project's `Phi`
bound instantiates. **Given this, §11.5's contribution is placed on the quantitative closed-form
characterization of the `c_warm` ceiling (the linear law, leading-order dependence, and envelope
characterization), not on the qualitative trend direction, which is prior art and must be cited
as such.**

## 10. Reproduce

```
uv run python beliefgrid2d_demo.py       # 2D solver validation (§1)
uv run python switching_curves_demo.py   # P1/P2/P3/G1/G2, writes output/switching_curves_data.json
uv run python nhop_demo.py                # n-hop generalization (§6), n=3
uv run python rho_perturbation_demo.py    # exact kappa recursion + O(rho^2) check (§7), ~2 min
uv run python adversarial_search_demo.py  # counterexample to G1 routing monotonicity (§4), ~3 min
uv run python zero_crossing_check_demo.py  # does G1's counterexample break the routing POLICY's
                                            # threshold, or just the d-field? (§4), single witness, ~1 min
uv run python zero_crossing_sweep_demo.py  # same check across all 12 violating trials (§4), ~1 min
uv run python localize_violations_demo.py  # localize + resolution-stable dip-depth metric (§4), ~2 min
uv run python open_region_check_demo.py    # per-axis + joint perturbation openness check (§4), ~3 min
uv run python always_cold_adversarial_search_demo.py  # minimal-model (2-action) counterexample (§4), ~3 min
uv run python invariant_features_demo.py   # per-scenario feature table for invariant-hunting (§4), ~3 min
uv run python voi_margin_gate_demo.py      # gate check for a sufficient-condition proof attempt (§4), ~1 min
uv run python fallback_policy_certify_demo.py  # #49(c) 2-parameter fallback policy certification (§4), ~10-20 min
uv run python wedge_wobble_decomposition_demo.py  # explains the beta2~0.2 phi wobble (§4/#50), ~10 min
uv run python invariant_candidates_demo.py  # rank candidate invariants by AUC (§4/#56), <1 min
uv run python holdout_validate_demo.py      # validate the invariant on fresh data (§4/#57), ~5-10 min
uv run python oned_always_cold_demo.py      # 1D single-hop reduction, no violation found (§4/#62), ~1 min
uv run python policy_multicrossing_targeted_search_demo.py  # finds a real policy multi-crossing (§4/#63), ~15-30 min
uv run python verify_policy_multicrossing_demo.py  # resolution-convergence check on that finding (§4/#63), ~3 min
uv run python policy_multicrossing_interior_search_demo.py  # gate check for #64's refined conjecture, ~30-45 min
uv run python calibrated_box_certificate_demo.py  # loose diagnostic only -- NOT a certificate, see §4/#65 for why, ~15-20 min
```

## 11. The warm/cold boundary itself: pure-Gilbert closed form and real-data positioning (2026-07-19)

Sections 1-10 characterize the always-warm sub-model's *policy* (the switching curve within a
fixed warm/cold-standby regime). This section answers a different, longstanding question: given a
choice between running warm standby at all (paying `c_warm`/step for a live probe) versus cold
standby (paying nothing per-step but a blind `c_switch_cold` penalty on re-entry), where is the
boundary between the two regimes, as a function of `(cost_a, c_warm, c_switch_warm, c_switch_cold,
channel parameters)`? `WARM_COLD_BOUNDARY_NOTES.md` established this numerically (`Phi(lambda)`
clean to `lambda=0.99`, an aggregation-hypothesis rejection, a resolved mean-shift-confound
artifact). This section adds a genuine closed-form answer for a special case, plus a decisive
real-data positioning result. Full derivation trail, including several corrected false starts, is
in `WARM_COLD_PURE_GILBERT_NOTES.md` — this section states only the validated conclusions.

### 11.1 The pure-Gilbert reduction

Restricting to `eps_good=0, eps_bad=1` (loss deterministically reveals the hidden channel state)
collapses the always-warm sub-model from a continuous-belief POMDP to a **finite, fully-observed
8-state average-cost MDP** (state = last-observed joint channel state x current context,
`pure_gilbert_finite_mdp_demo.py`), solvable by exact policy iteration — no grid, no RVI
truncation error. Cross-validated against the continuous solver to `1e-10`-`1e-17` at four random
parameter points. The always-cold sub-model similarly reduces to a **semi-Markov renewal-reward**
process over an embedded 2-state chain (`{H, BB}` = one-hop-bad / both-hops-bad re-entry types,
`pure_gilbert_symbolic_cold_demo.py`), giving an exact `g_cold(n_H, n_BB)` in closed form (`n_H,
n_BB` = park-duration decay exponents), validated against the continuous solver to `2e-10`.

Three warm-side closed forms (always-A, always-B, route-B-iff-GG), independently re-derived via
sympy from the channel's own transition kernel rather than transcribed, all matched by direct
symbolic comparison:

```
g(always-A)       = cost_a + c_warm
g(always-B)       = c_warm + pi_b*(2 - pi_b)
g(route-B-iff-GG) = c_warm + (1-pi_b)^2*(1-q_G^2)*(1+2*c_switch_warm) + (1-(1-pi_b)^2)*cost_a
```
where `pi_b = P(Bad)`, `lambda` = single-hop persistence eigenvalue, `p_gb = pi_b*(1-lambda)`,
`q_G = 1-p_gb`.

### 11.2 Cold-side threshold structure, and a resolved corner case (`pi_b>1/2`)

The cold side's optimal park duration exhibits a genuine, closed-form-characterized non-
monotonicity for `pi_b>1/2` (P(GG|return) has an interior maximum at `x*=(2*pi_b-1)/(2*pi_b)`,
producing a real interior-optimum park duration in a specific `cost_a` window near the stationary
loss) — fully derived, including a unified detachment-threshold formula for both re-entry types
(`cost_a*(entry) = (1+2*c_switch_cold)/(1+N*P_GG^max(entry))`, `N=1/(1-q_G^2)`) and a proof that
the one-hop-bad entry always detaches first (`cost_a*_H < cost_a*_BB` whenever `pi_b>1/2`). **For
`pi_b<=1/2` — the regime both of this project's real calibrated hops fall in (Berlin V2X: `pi_b=
0.2954, 0.4127`, see §11.3) — this collapses to a single shared threshold and simple monotone
behavior, no multimodality.** The `pi_b>1/2` case is recorded as a resolved but empirically
inapplicable corner case; see `WARM_COLD_PURE_GILBERT_NOTES.md` for the full derivation, including
a documented resolution artifact (a grid search that appeared to show a "two-stage" detachment
turned out to need the search range pushed several orders of magnitude further before the true,
single-gated detachment threshold became visible — a standing methodological lesson recorded
there for any future numeric work on this reduction).

### 11.3 Phi=0 closed form (primary active cell) and the decisive real-data result

Numerically tracing `Phi = g_warm* - g_cold*` against `cost_a` (both sides via their respective
exact finite algorithms) identifies the policy pair active at the crossing for `pi_b<=1/2`: warm
switches from always-A to route-B-iff-GG before cold leaves its always-A-cold plateau, so the
crossing itself is governed by `g(route-B-iff-GG)` vs. the trivial `g_cold=cost_a` plateau value.
Solving `Phi=0` in that cell gives a genuine closed form:

```
cost_a* = c_warm/(1-pi_b)^2 + (1 - q_G^2)*(1 + 2*c_switch_warm)
```

verified to 5 decimal places against the exact 8-state policy-iteration solver
(`warm_cold_phi_zero_active_cell_demo.py`, `warm_cold_phi_zero_closed_form_derivation_demo.py`).
Reading: the boundary balances the savings warm standby earns by riding path B during the fraction
`(1-pi_b)^2` of time both hops are Good against the probe cost `c_warm` paid every step regardless
— so warm is worthwhile only once `cost_a` clears a level inversely proportional to how rare that
good window is. A clean, exactly-verified anchor holds throughout the always-A/always-A-cold
regime at low `cost_a`: `Phi = c_warm` exactly, with no other structure — cold there simply saves
the warm probe's own cost, nothing more.

**This pure-Gilbert closed form is an idealized anchor, not a literal predictor**: real fitted
channels have `eps_good`/`eps_bad` far from `0`/`1` (Berlin V2X: `~0.03-0.07`/`~0.30-0.43`) and
asymmetric hops, so the real question is answered directly by the project's general, asymmetric,
partial-observation solver (`switching_curves.always_warm_value_iteration` /
`always_cold_value_iteration`, unchanged from §1-§4) evaluated on the real fitted parameters, not
by force-fitting the symmetric closed form onto them.

Doing exactly that (`warm_cold_phi_zero_real_data_position_demo.py`), at this project's peak-gain
operating point (`c_warm=0.005, c_switch_warm=0.01, c_switch_cold=0.02`, `TRACE_CALIBRATION_NOTES.md`'s
"peak relative value 0.65%" point) on the real Berlin V2X EM-fitted hops: **the calibrated real
`cost_a=0.30` sits almost exactly on the `Phi=0` crossing.** The window where the fixed always-
warm policy actually beats fixed always-cold is only `cost_a in [~0.2975, ~0.3075]` wide (about 3%
of its own value), with the deepest point of the dip (`Phi~-0.00057`, i.e. `|Phi|/cost_a<0.2%`)
almost exactly at the independently-calibrated real `cost_a=0.30` — not tuned to land there.
Outside this narrow window, `Phi` returns quickly to exactly `c_warm` on both sides (both very low
and very high `cost_a` degenerate to one side's fixed policy dominating trivially, same clean
anchor identity as §11.1's low-`cost_a` case, confirmed here in the fully general/asymmetric/
partial-observation setting too).

**Important scoping, per external review**: `Phi` (fixed-warm vs. fixed-cold) and the adaptive
gain (adaptive-optimal vs. best-of-the-two-fixed-policies) are *different quantities* — adaptive
policies form a strictly richer class than either fixed policy, so a near-tie between the two
fixed policies does not, by itself, *prove* the adaptive gain must be small; in principle an
adaptive policy could still beat both fixed policies by a wide margin even when they are tied
with each other. The rigorous, load-bearing claim in this project remains the **directly measured**
adaptive gain (§10, via `beliefgrid2d.belief_grid2d_value_iteration_warm` against the better of the
two fixed policies). `Phi=0`'s role is explanatory context — an analytical account of *why* the
gap is small at this operating point — not a proof that it must be.

**Robustness check** (`warm_cold_robustness_sweep_demo.py`): to rule out the "exactly on the
boundary" landing being a fragile coincidence of the specific calibrated point (a natural reviewer
objection), perturbed `cost_a` and all four parameters of both hops (`p_gb, p_bg, eps_good,
eps_bad`) independently by `+/-20%` (80 samples) and re-measured BOTH `Phi` and the directly-
measured adaptive gain at every perturbed point. Result: adaptive gain stayed under `5%` in `100%`
of the 80 samples (max `0.756%`, mean `0.087%`, vs. `0.649%` at the unperturbed calibrated point),
and `|Phi|` never exceeded `c_warm=0.005` in any sample.

**This `+/-20%` figure was checked, not asserted, against real calibration uncertainty** — a
block bootstrap over each hop's raw per-epoch window sequence (`berlin_v2x_bootstrap_ci_demo.py`,
8 resamples/hop, block=20 windows matching the project's own established autocorrelation decay
scale, refitting the Binomial-HMM EM on each resample) found the flat `+/-20%` figure is actually
NOT conservative for several parameters — bootstrap std/point-estimate ratios ran up to `28.8%`
(hop2 `eps_good`; its full bootstrap range spanned `95%` of its point estimate across just 8
resamples) and `11.7%-14.5%` for both hops' `p_gb`. **Rather than under-claiming robustness with an
unjustified perturbation width, re-ran the sweep with per-parameter widths set to roughly
`2*(bootstrap std/point estimate)`** (floored at the original `20%`, so strictly wider, never
narrower, than the first sweep) — up to `+/-57.6%` for hop2's `eps_good`, `+/-29%` for hop1's
`p_gb`/`eps_good`, `+/-23.4%` for hop2's `p_gb`
(`warm_cold_robustness_sweep_v2_bootstrap_calibrated_demo.py`). **The finding not only survived
this much wider, empirically-grounded box, it strengthened**: adaptive gain stayed under `5%` in
`100%` of a fresh 80 samples, and in fact under `1%` in all 80 (`max=0.885%`, `mean=0.097%`,
`median=0.000%`) — comparable to, not worse than, the original flat-`20%` sweep, despite one
parameter's perturbation width nearly tripling.

**The `median=0.000%` is itself the empirical face of the `Phi<=c_warm` theorem below, not a
separate coincidence**: widening the perturbation box pushes most sampled points OUT of the narrow
`Phi~0` window and into a region where one fixed policy cleanly dominates the other — precisely the
zero-switch-rate regime where the theorem's bound is tight (`Phi=c_warm` exactly) and the
adaptive-optimal policy IS that dominant fixed policy, so the measured gain over the best fixed
policy is exactly `0`. The theoretical bound (tier (i) below) and the empirical median (tier (iii))
are two views of the same structural fact, not independent lines of evidence that happen to agree.

**The correct framing is therefore: the real operating point sits in a neighborhood that is
robustly low-gain under real (bootstrap-estimated, not merely asserted) calibration uncertainty —
not that it lands exactly on a knife-edge boundary** (the "almost exactly on Phi=0" observation at
the single calibrated point is a striking illustration, kept as such, but the paper-level claim
rests on the neighborhood-wide robustness result under the wider, empirically-checked
perturbation box, not on the single point or an unverified perturbation width).

**`Phi<=c_warm` is a provable structural bound, not an empirical coincidence** (the reason the
robustness sweep found `max(Phi)=c_warm` exactly and never higher, in all 80 samples): take
whatever action sequence the always-cold-optimal policy produces, and replay that EXACT SAME
decision rule under the always-warm setup (i.e. ignore the extra live-probe information the warm
setup provides, and just execute cold's routing/switching decisions blindly). This is a FEASIBLE
warm-setup policy (not necessarily optimal). **One subtlety, caught in review**: switching cost is
a property of the ENVIRONMENT (warm vs. cold), not of the policy — so a switch in this replayed
sequence is charged `c_switch_warm` (the warm setup's own rate) rather than `c_switch_cold`, even
though the decision to switch was copied from cold's policy. Since the decision rule is identical,
the replay's expected routing loss and its switch RATE exactly match `g_cold*`'s own, but the
switch COST differs:

```
replay value = E[routing loss] + (switch rate)*c_switch_warm + c_warm
g_cold*      = E[routing loss] + (switch rate)*c_switch_cold
```

so `replay value = g_cold* + c_warm - (switch rate)*(c_switch_cold - c_switch_warm)`. Since the
true warm-optimal policy can only do at least as well as this one feasible candidate,

```
Phi = g_warm* - g_cold* <= c_warm - (switch rate)*(c_switch_cold - c_switch_warm)
```

**This is `<=c_warm` exactly when `c_switch_cold >= c_switch_warm`** — a condition that holds
physically always in this project (cold's switch cost includes the warm-up/reconnection overhead
that warm's switch, already primed by the live probe, does not need to pay; every calibrated value
used in this project satisfies it, e.g. `c_switch_warm=0.01 < c_switch_cold in {0.02,0.10,0.5}`
throughout `TRACE_CALIBRATION_NOTES.md`). Under that condition the bound `Phi<=c_warm` is not just
true but for ANY channel model or cost structure satisfying `c_switch_cold>=c_switch_warm`, not
just pure-Gilbert — a domination/feasibility argument, independent of the rest of this section's
machinery. **The refined bound also explains its own tightness**: equality (`Phi=c_warm` exactly)
holds precisely when the switch rate is `0` — i.e. the always-A/always-A-cold and
always-B/always-B-cold degenerate regimes at the low/high `cost_a` extremes (§11.1/§11.3's anchor
identity) — and `Phi` sits strictly BELOW `c_warm` whenever any switching actually occurs. This is
exactly why the robustness sweep's `max(Phi)` sat precisely at `c_warm` rather than merely being
bounded by some looser empirical ceiling: those samples are the ones landing in a
no-switching-needed regime. **Combined with the robustness sweep, this gives the full three-part
story**: (i) `Phi` is STRUCTURALLY capped at `c_warm` from above whenever `c_switch_cold>=
c_switch_warm` (proven, general, physically satisfied here), with the cap strictly loosening as
switching becomes cheaper relative to cold; (ii) for THIS project's real, calibrated channels, the
dip below zero is shallow because `c_warm/cost_a` is itself small (§11.1's physical-driver
argument); (iii) EMPIRICALLY, both properties were verified to survive perturbation of every fitted
parameter, including at bootstrap-derived (not just asserted) uncertainty widths (see below).
Robustness is not luck — it follows from (i)+(ii) as much as it is independently confirmed by
(iii).

**Independently re-verified** by a fresh agent with no access to this derivation, writing its own
script from scratch against the same real hop parameters: `g_warm=0.29917536, g_cold=0.29957952,
Phi=-0.00040417` (inside the claimed range), `g_adaptive=0.29720598, gain=0.6583%` (inside the
claimed range, well under 5%). **One caveat this re-verification surfaced**: `Phi`'s precise
numeric value is somewhat solver-resolution-sensitive (`-0.00031` at `resolution=40` vs.
`-0.00054` at `resolution=80`, roughly a `1.7x` spread) — its SIGN and rough order of magnitude are
stable across resolutions, but its exact value should not be over-quoted to more than 1
significant figure. The adaptive **gain** figure, by contrast, was stable across resolutions
(`0.6375%` to `0.6588%`) — reinforcing that the gain (the actually load-bearing quantity per point
1 above) is the more robust number of the two, while `Phi` is best treated as an order-of-magnitude
explanatory quantity, not a precise landing coordinate.

**Why**: in this fully general partial-observation setting (unlike the pure-Gilbert reduction of
§11.1, where belief collapses to exact simplex corners and both sides are solved with NO grid
interpolation at all — which is why those closed forms matched the continuous solver to machine
precision), belief lives at genuine interior points of the simplex, so `always_warm_value_
iteration`/`always_cold_value_iteration` both carry ordinary grid-interpolation error in their `.g`
output. `Phi` is the small DIFFERENCE of two `~0.299`-scale quantities (`~0.0004`), so that shared
interpolation error's relative weight is amplified by the subtraction — hence resolution-
sensitive. The adaptive **gain**, by contrast, compares the adaptive solution against
`min(g_warm,g_cold)` computed under the SAME grid/resolution, so the two sides' shared
interpolation error tends to cancel rather than amplify — hence stable. (This is the correct home
for an early, more general "grid-interpolation-error" concern raised earlier in this line of
investigation: it does not apply to §11.1's pure-Gilbert closed forms, which involve no
interpolation at all, but it does explain a genuine, now-quantified numerical property of `Phi` in
this section's fully general real-data setting.)

**The physical driver, stated as a general design principle rather than a one-off finding**: this
whole picture is driven by `c_warm/cost_a` being small (`~0.005/0.30 ~ 1.7%` at the calibrated
point) — the warm probe is cheap relative to the cost of routing through the bad path. The general
statement (visible directly in the low-`cost_a` anchor identity `Phi=c_warm` and in the `cost_a*`
closed form's `c_warm/(1-pi_b)^2` term, §11.1/§11.3): **whenever `c_warm/cost_a` is small for a
given deployment, the adaptive gain over the better fixed policy is structurally bounded to be
small too, and this project's real measured timing (task #2/#3's calibration of why the warm
probe is cheap for this project's QUIC path-validation mechanism) is why that condition holds
here.** A deployment with a substantially more expensive warm-probe mechanism (larger `c_warm/
cost_a`) would not be covered by this conclusion — the negative result is conditional on this
project's own real cost structure, not universal, and is stated that way deliberately so it reads
as a general design principle rather than an artifact specific to Berlin V2X.

### 11.4 Why the boundary LOCATION cannot be distilled into a simple approximate formula — and what can be

A natural engineering objection to §11.1's pure-Gilbert closed forms: an EXACT result confined to
an idealized special case (`eps_good=0, eps_bad=1`) is less useful than a simple APPROXIMATE
formula with an honestly-quantified small error across the realistic range. This was tested
directly: substitute a real hop's own `p_gb, p_bg` (hence `pi_b, lambda`) into the pure-Gilbert
`cost_a*` closed form (§11.3) UNCHANGED — ignoring that its real `eps_good, eps_bad` are nowhere
near `0, 1` — and check how far the prediction is from the TRUE crossing (found via the general,
eps-aware `switching_curves` solver).

**Result: this failed, and not gracefully.** Moving `eps` even a small fraction of the way from
`(0,1)` toward a real hop's own calibrated values caused the predicted crossing to VANISH
entirely (no sign change found across a wide `cost_a` search), rather than drifting with a
gradually growing error. A natural next idea — expand `g_warm*(eps)`/`g_cold*(eps)` in a Taylor
series around the pure-Gilbert point and correct the closed form to first order — turns out to be
a dead end for a structural reason, not merely a computational one: the finite-MDP/semi-Markov
reduction of §11.1 exists ONLY exactly at `eps=(0,1)` (deterministic observation collapses belief
to a finite reachable set); anywhere else, belief wanders a continuous simplex (a genuinely
continuous-belief POMDP). Moving `eps` away from `(0,1)` is therefore a SINGULAR perturbation — the
reachable-belief-set's cardinality changes discontinuously (finite to continuous) — so no
closed-form Taylor coefficient exists at all; obtaining even the first-order correction would
itself require solving the continuous-belief POMDP, defeating the purpose of a cheap formula.

**The deeper reason (confirmed directly, not just argued)**: the crossing LOCATION is
fundamentally **ill-conditioned**, independent of which approximation method is attempted. Since
`Phi<=c_warm` always (§11.3's structural bound), and the real dip below this ceiling is tiny
(depth `~0.0006` at the calibrated point), the crossing is where two near-tied curves meet at a
shallow angle — any perturbation to the inputs can move the crossing far or erase it entirely.
Tracing `Phi(cost_a)` at intermediate `eps` levels (interpolating from `(0,1)` toward a real hop's
calibrated values) confirms the valley does NOT misbehave: it keeps the SAME shape, merely shifting
upward continuously until it no longer dips below zero at all (a graceful "parallel shift," not a
pathology) — at hop1's real `eps=(0.032,0.301)`, `Phi` sits at essentially exactly `c_warm=0.005`
from `cost_a=0.02` to `~0.18`, dipping only to `0.004537` (`90.7%` of the ceiling) at `cost_a=0.20`,
never reaching zero.

**The correct reframe: do not distill the (ill-conditioned) boundary LOCATION — distill the
(well-conditioned) GAP BOUND that was already in hand.** `Phi<=c_warm` (§11.3) uses no
channel-model assumption whatsoever — its proof is a pure policy-domination argument, holding
identically for pure-Gilbert and general partial-observation channels. This already IS the
"simple formula with rigorously small (here, PROVEN ZERO) error, valid generally" that motivates
the objection above — it was simply aimed at the wrong target (the crossing's location) rather
than the right one (the ceiling on the gap).

**This reframe produces a stronger empirical statement of the real-data result.** Sweeping
`Phi(cost_a)` over a WIDE range (`cost_a` in `[0.02, 0.60]`, `warm_cold_wide_range_ceiling_check_
demo.py`) for both hop2 alone and the actual asymmetric real pair (hop1+hop2, exactly §11.3's
setup): `Phi` sits at `>=90%` of the proven `c_warm` ceiling across `63.3%` of the swept range, and
dips meaningfully below the ceiling ONLY in the narrow window `cost_a in [~0.285,~0.360]` already
identified in §11.3 — even there, the deepest point reaches only `-8.1%` of `c_warm` in magnitude
(`Phi=-0.000404` at `cost_a=0.300`). For hop2 alone (symmetric), `Phi` sits at EXACTLY `c_warm`
across the ENTIRE tested range, with only a shallow non-crossing dip near `cost_a=0.40`. **Rather
than "the real operating point sits near a boundary," the more accurate and more general
statement is: in the realistic `eps` regime, cold weakly dominates warm by (very close to) the
full `c_warm` amount across almost the entire plausible `cost_a` range — there is barely any
window left anywhere for an adaptive policy to exploit, not merely a narrow one nearby.**

A general, broadly reusable methodological point falls out of this exercise: **when two competing
fixed policies are near-tied, the LOCATION of their crossing is fundamentally ill-conditioned and
not robustly approximable by any method (substitution, Taylor expansion, or otherwise), but the
BOUND on their gap is well-conditioned and can be stated universally.** What is actually useful for
engineering design in such settings is not the boundary's exact location, but the bound on the gap.

```
uv run python pure_gilbert_closed_form_as_approximation_v2_demo.py   # naive substitution failure, quantified (§11.4)
uv run python warm_cold_wide_range_ceiling_check_demo.py             # Phi hugging the c_warm ceiling, wide range (§11.4)
```

```
uv run python pure_gilbert_finite_mdp_demo.py                        # always-warm exact reduction (§11.1)
uv run python pure_gilbert_symbolic_warm_demo.py                     # warm closed forms, sympy-verified (§11.1)
uv run python pure_gilbert_symbolic_cold_demo.py                     # cold g_cold(n_H,n_BB) closed form (§11.1)
uv run python pure_gilbert_nh_threshold_derivation_demo.py           # n_H* bump-detachment threshold (§11.2)
uv run python pure_gilbert_nbb_threshold_bracket_check_demo.py       # n_BB* detachment threshold (§11.2)
uv run python pure_gilbert_cold_full_embedded_sweep_demo.py          # full joint (n_H,n_BB) confirmation (§11.2)
uv run python pure_gilbert_coupled_threshold_bisection_demo.py       # coupled joint detachment, resolution-corrected (§11.2)
uv run python warm_cold_phi_zero_active_cell_demo.py                 # Phi=0 active-cell identification (§11.3)
uv run python warm_cold_phi_zero_closed_form_derivation_demo.py      # Phi=0 closed form, sympy-verified (§11.3)
uv run python warm_cold_phi_zero_real_data_position_demo.py          # real Berlin V2X Phi positioning (§11.3), ~2-3 min
uv run python warm_cold_robustness_sweep_demo.py                     # +/-20% perturbation robustness check (§11.3), ~1 min
uv run python berlin_v2x_bootstrap_ci_demo.py                        # block-bootstrap calibration-uncertainty estimate (§11.3), ~5-8 min
uv run python warm_cold_robustness_sweep_v2_bootstrap_calibrated_demo.py  # robustness re-check at bootstrap-derived (wider) widths (§11.3), ~1 min
```

### 11.5 Where does warm actually dominate? A phase diagram over the meta-parameters (2026-07-19)

Sections 11.1-11.4 characterize the boundary at fixed, calibrated meta-parameters. This section
answers a broader question: across the meta-parameter space itself
(`pi_b, lambda, c_warm, c_switch_warm, c_switch_cold`), where does a warm-win window exist at all,
how wide/deep is it, and what governs its disappearance? Built the exact pure-Gilbert phase
diagram (no belief-grid resolution error at all, per §11.1's finite reduction): for each
`(pi_b, lambda)`, found the full window `[cost_a_lo, cost_a_hi]` in which the always-warm-optimal
fixed policy beats the always-cold-optimal fixed policy.

**A real bug was caught and fixed during this exercise, worth recording as a caution**: the first
attempt's cold-side solver compared the finite-park semi-Markov optimum against only ONE
degenerate plateau candidate ("always route A forever," value `cost_a`) and omitted the OTHER
candidate, "always route B forever" (value = the stationary `path_b_loss = pi_b*(2-pi_b)`), which
becomes cold's TRUE optimum once `cost_a` exceeds that stationary loss. This overestimated
`g_cold` and produced a spuriously wide-and-deep fake window for most of the grid. Caught by
cross-checking against the already-trusted general `switching_curves` solver at one shared point
(a real, reproducible disagreement, confirmed not to be a solver-resolution artifact by testing
resolutions up to 250); fixed by taking the three-way minimum
`g_cold = min(finite-park joint-argmin, cost_a, pi_b*(2-pi_b))`.

**Corrected phase diagram** (`c_warm=0.02, c_switch_warm=0.01, c_switch_cold=0.02`):

```
pi_b\lambda   0.2        0.4          0.6          0.8
0.1        no window   no window   depth 22.6%   depth 39.2%
0.2        no window   depth 64.0% depth 124.5%  depth 147.9%
0.3        ~0 (knife-edge) depth 120.2% depth 193.2% depth 218.8%
0.4        no window   depth 138.6% depth 223.1%  depth 253.2%
0.5        no window   depth 107.0% depth 211.4%  depth 246.9%
```
(depth as a fraction of `c_warm`; "no window" means no `cost_a` exists at which warm-fixed beats
cold-fixed at all)

**The corrected trend REVERSES a wrong first-pass reading caused by the bug above**: window depth
(and its very existence) INCREASES with `lambda` (more persistence favors warm, not less) and
generally increases with `pi_b` (worse average channels favor warm). Low `lambda` (fast
decorrelation) frequently erases the window entirely. **Mechanistic reading**: warm's edge comes
from tracking and exploiting PERSISTENT stretches of the Good state precisely; the more persistent
the channel, the more a blind guess risks parking through (or returning mid-way into) a long
favorable or unfavorable stretch, and the more continuous monitoring is worth its `c_warm` cost. A
near-i.i.d. channel has little such structure to exploit, so warm's information advantage shrinks
toward insignificance.

**Novelty check on this qualitative trend (web literature + advisor consultation)**: this
direction is NOT new — it is already established in the opportunistic-spectrum-access/restless-
bandit-over-Gilbert-Elliott literature, most directly **"Optimal Power Allocation over Two
Identical Gilbert-Elliott Channels" (arXiv:1210.3609)** (a two-Gilbert-Elliott-channel structure
closely matching this project's own two-hop setup) and **Zhao, Krishnamachari & Liu, "Myopic
Sensing for Multi-Channel Opportunistic Access" (arXiv:0712.0035)**, which classically formalize
how channel persistence governs the value of sensing/exploitation. **Do not cite plain
Age-of-Information (AoI) literature as precedent for this direction** — AoI's own monotonicity
runs the opposite way (fast-changing sources need MORE frequent refresh), unlike §11.2's earlier
`pi_b>1/2` finding, where AoI (arXiv:2403.03380) WAS the correct prior art. General
value-of-information-in-MDP framings (arXiv:2201.07908) and time-correlated-channel remote-
estimation scheduling (arXiv:2303.16285, 2403.13898) are also relevant background. **Given this,
the contribution is placed on the QUANTITATIVE closed-form characterization below, not the
qualitative trend, which is prior art.**

**The `c_warm` ceiling: an exact linear law, a leading-order closed form, and a general envelope
characterization — deliberately NOT a single closed form.** At fixed `(pi_b, lambda)`, sweeping
`c_warm` (`warm_win_c_warm_scaling_demo.py`, bug-fixed) shows the window shrinking and eventually
vanishing entirely above a finite ceiling `c_warm_vanish`. Three findings, in order of rigor:

1. **Exact value via a linear law.** Since the warm regime pays `c_warm` every step regardless of
   routing policy, `c_warm` enters `Phi` PURELY ADDITIVELY across every active cell:
   `Phi(cost_a; c_warm) = c_warm + Psi(cost_a)`, `Psi` entirely `c_warm`-independent. Hence the
   window's depth is EXACTLY LINEAR in `c_warm` (`depth(c_warm) = c_warm_vanish - c_warm`), so
   `c_warm_vanish` can be read directly off ANY single computed point via
   `c_warm_vanish = c_warm + depth` — no re-sweep needed. Verified to 4 significant figures across
   4 independently-computed `c_warm` values (`pi_b=0.3, lambda=0.4`): all converge on
   `c_warm_vanish ~= 0.0440`, independently cross-checked by directly locating `Psi`'s numeric
   minimum (`cost_a~=0.456, Psi_min=-0.044127`) — two fully independent methods agreeing to
   `~0.0002`.
2. **Leading-order closed form (a lower bound, not exact).** Within the primary active cell,
   `Psi` is EXACTLY LINEAR in `cost_a` (verified symbolically: `dPsi/dcost_a = -(1-pi_b)^2`
   identically, no curvature at all within one cell) — so the window's "dip and return" shape
   comes ENTIRELY from transitions BETWEEN cells as `cost_a` grows, never from curvature within
   one. Using the nearest cell-transition boundary as a proxy gives
   `c_warm_vanish_leading = (1-pi_b)^2*(cost_a_boundary - K)`,
   `K=(1-q_G^2)*(1+2*c_switch_warm)`, correct in order of magnitude and `(pi_b,lambda)`-dependence
   (including the clean limit `lambda->1 => K->0 => c_warm_vanish` rising to its maximum — a
   quantitative sharpening of the qualitative trend above) but falling short of the exact value by
   `~8%`, confirmed to be a real, non-noise gap.
3. **A general envelope characterization explains the 8% gap and is itself the genuine novel
   contribution.** By the envelope theorem (already used in §7.2 of this document for a different
   purpose), `dPhi/dcost_a` at any active cell's optimum equals that policy's OWN realized
   fallback-path (`A`) usage fraction — a fact holding for ANY channel model, not just
   pure-Gilbert. The valley bottom (warm's point of maximal advantage) therefore occurs EXACTLY
   where the two optimal policies' `A`-dwelling fractions coincide. Verified directly: tracking
   cold's own optimal `(n_H^*, n_BB^*)` near the valley (`pi_b=0.3, lambda=0.4`), its `A`-fraction
   jumps discretely from `0.560` to `0.484` exactly at `cost_a=0.456` (the true valley bottom) as
   `n_H^*` steps from `4` to `3` — and warm's own (fixed) `A`-fraction, `pi_b*(2-pi_b)=0.51`, falls
   precisely BETWEEN these two straddling values. This confirms the continuous envelope condition
   governs the true (integer-constrained) valley bottom almost exactly, and explains the 8% gap:
   right at cold's naive detachment threshold, its own `A`-fraction is still close to `1` (park
   length still effectively unbounded there) — the true valley lies further into the finite-park
   cell, where cold's `A`-fraction has had room to fall to the matching value.

**Why not chase a single exact closed form further**: the envelope condition
`A_frac_cold(cost_a)=pi_b*(2-pi_b)` is only implicit, since `A_frac_cold` depends on cold's own
optimal park length `n^*(cost_a)`, which (per §11.2's extensive `pi_b>1/2` investigation) has no
clean closed form in general (transcendental/fold-at-infinity structure). Solving the envelope
condition exactly would reopen that whole machinery for an `8%` correction — precisely the kind of
disproportionate closed-form-chasing this document's own standing principle (state a finite/exact
numeric characterization honestly rather than force an ugly closed form) argues against. The
three-part characterization above — exact value (free), leading-order dependence (a lower bound,
labeled as such), general structural explanation (the genuine new contribution relative to the
already-known qualitative trend) — is adopted instead.

**Systematic error quantification of the leading-order form, across the full `(pi_b,lambda)` grid
(not just the single point above)**: the `8%` figure at `pi_b=0.3,lambda=0.4` turns out to be a
middling, non-representative case. Repeating the leading-order-vs-exact comparison at all 20 grid
points from §11.5's phase diagram gives a mean relative error of `20.8%`, median `13.7%`, and a
range of `[-3.3%, +65.4%]` — the error is emphatically NOT uniform. It is smallest (a few percent,
occasionally even a small negative — the leading-order form is only APPROXIMATELY a lower bound,
not strictly one) when `pi_b` is large and `lambda` is small-to-moderate (best: `pi_b=0.4-0.5,
lambda=0.2-0.4`), and largest (up to `65%`) when `pi_b` is small and `lambda` is large (worst:
`pi_b=0.1, lambda=0.8`). **Mechanistic reason**: the true valley bottom requires cold's own
`A`-dwelling fraction to fall from near-`1` (at its naive detachment point) down to warm's fixed
value `b^2=pi_b*(2-pi_b)`; when `pi_b` is small, `b^2` is itself small, so a much larger excursion
into the finite-park cell (in `cost_a`-space) is needed to reach it, stretching the gap between the
naive estimate and the true valley — and larger `lambda` stretches this same transition further,
compounding the effect. **Practical conclusion**: the leading-order closed form should be trusted
for its qualitative `(pi_b,lambda)`-dependence and the `lambda->1` limiting behavior, but NOT for a
precise numeric value, especially for `pi_b<=0.2` where 30-65% errors are common — the exact
numeric value (free, via the linear law) should always be used instead when a real number matters.

**Why `lambda` specifically stretches the transition (a supporting mechanism, checked and
right-sized, not pursued as a standalone result)**: near cold's detachment threshold (a
fold/saddle-node-type transition between "park forever" and a genuine finite optimum), the
optimal park duration `n^*` is finite and well-determined (confirmed directly — an initial
reading that `n^*` was diverging/unresolved at high `lambda` turned out to be a search-range
artifact, caught by widening the search ceiling from `30` to `10,000` and finding a finite
`n^*=44`, not an unbounded one), but the OBJECTIVE'S FLATNESS around that optimum (the ratio of
value-variation across a window of candidate durations to the optimum's own depth below the
plateau) is measurably and consistently larger for high-`lambda` channels than low-`lambda` ones
at matched distances from detachment (roughly `10x` larger in the tested case,
`pi_b=0.3`, comparing `lambda=0.8` to `lambda=0.2`) — i.e. this is a genuine but CONTINUOUS
crossover-width effect, not a distinct third "phase" or a genuine indifference region. This
directly explains why the leading-order approximation degrades faster with `lambda`: a flatter
near-optimum takes a wider `cost_a` range to resolve into a value close to the true valley bottom.
The general shape of this fact (value well-determined, optimal control comparatively flat/
non-unique near a fold-type indifference point) is itself not new — it echoes the classical
Skiba/DNSS-point literature in optimal control and the well-known ill-definedness of Whittle
indices near restless-bandit indexability boundaries — so this is recorded here only as a
supporting explanation for the error pattern above, not pursued as an independent contribution.

```
uv run python c_warm_vanish_approximation_error_demo.py      # systematic leading-order error quantification (§11.5)
uv run python warm_win_phase_diagram_pure_gilbert_demo.py    # exact warm-win window map over (pi_b,lambda) (§11.5)
uv run python warm_win_c_warm_scaling_demo.py                # c_warm scaling + vanishing ceiling (§11.5)
uv run python warm_win_eps_tolerance_v2_demo.py              # how far eps departure the window tolerates (§11.5)
```
