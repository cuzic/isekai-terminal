# Stage 0 report: hop-decomposed path quality for a dual-mobility relay

Research plan: https://claude.ai/code/artifact/fb703651-d3fc-423a-b759-f56b649deb8e
Code: `research/dual-mobility-relay/` on branch `research/dual-mobility-relay`
(`dmr/` package + `run_stage0.py` demo + `sweep.py` parameter sweep)

## Model

- Each hop (drone↔car UHF/Wi-Fi = hop1, car↔WAN = hop2) is a 2-state
  Gilbert-Elliott Markov channel (`dmr/channels.py`). Joint state = 4 states
  `{GG,GB,BG,BB}`. Inter-hop correlation `rho` interpolates between
  independent hop dynamics and hop2 being forced to mirror hop1.
- Two observation models on path B: **composite** (1 bit: packet lost
  somewhere on the path) vs. **hop-decomposed** (2 bits: which hop lost it).
  `o_composite` is a deterministic coarsening of `o_decomp`, so
  `I(X;O_decomp) ≥ I(X;O_composite)` always (data processing inequality).
- Routing MDP: choose path A (direct cellular, fixed loss `cost_a`) or path
  B (relay, loss = hop1 ∨ hop2 loss) each step. Exact value iteration
  (`dmr/mdp.py`), exact HMM belief filtering (`dmr/filtering.py`), exact MI
  computation (`dmr/information.py`).
- CUSUM hop-attribution detector (`dmr/changepoint.py`): per-hop
  log-likelihood-ratio CUSUM on the decomposed loss bits — only possible
  because the observation is decomposed, not the composite bit.

## Finding 1 — MI gap is real, always positive, and shrinks with correlation

`output/mi_gap_heatmap.png`: hop-decomposed observation always carries more
information about the true 4-state channel than the composite bit (as
guaranteed by the DPI), ranging ~0.014–0.053 bits across the swept region.
The gap **shrinks monotonically as inter-hop correlation (rho) grows**
(knowing hop1's state already implies hop2's state, so the second
observation adds less) and **grows with hop2's burst length** up to a point.
This part of Stage 0 is clean and unsurprising.

## Finding 2 — the naive (no switching cost) routing model gives *zero*
routing value from decomposition, despite positive MI

With a plain 2-action MDP where switching paths is free, the optimal action
whenever *either* hop is Bad is simply "bail to path A" — because path B's
loss under any single bad hop already exceeds path A's fixed loss by a wide
margin. The two single-hop-bad states (`GB`, `BG`) then share the same
optimal action, so knowing *which* hop is bad never changes the decision.
Composite and hop-decomposed belief policies come out numerically identical
(`run_stage0.py`, scenario 1). This is an important negative result: **the
plan's hypothesized asymmetry (bail on hop2, ride out hop1) requires
switching to have a real cost/hysteresis** — without it, there's no reason
not to bail on every degradation, hop identity be damned.

## Finding 3 — with a switching cost, the asymmetric policy appears, but the
routing-value gain is small and fragile

Adding a switching-cost-augmented MDP (`dmr/switching.py`: state = channel
state × currently-active-path, action-dependent transition) recovers the
hypothesized policy in a scenario tuned so hop1 degradation is brief/moderate
and hop2 degradation is long/severe:

```
state=GB (hop2 bad):  always bail to A, regardless of current path
state=BG (hop1 bad):  bail if currently on A, but *ride it out* if already on B
```

This is exactly the mechanism the plan's §4.2 hypothesized. But quantifying
its value (`sweep.py`, 6×5×3 grid over rho / hop2 burst length / switching
cost, `output/policy_gap_heatmap_c_switch_*.png`) shows:

- The realized routing-policy value gain from decomposition is **small**:
  roughly 0–19% of the theoretical ceiling (oracle vs. best-naive-fixed-path
  average cost), typically single digits.
- It is **highly sensitive to the switching cost**: too low and there's
  nothing to protect against by riding out hop1; too high and the belief
  policy gets essentially stuck on path A permanently (confidence needed to
  justify paying the switch-back cost is much larger than what a realistic
  partial-observation belief accumulates — see the razor-thin `Q(B)-Q(A)`
  margin at the fully-observed `GG` state), collapsing composite and
  decomposed policies to the same "bail once, stay bailed" behavior
  (several `c_switch=0.20` cells are exactly 0%).
- The heatmap has **no clean monotonic structure** (unlike the MI heatmap)
  and many cells are within 1–2 standard errors of zero at n_traj=120 Monte
  Carlo trajectories — i.e. this sweep is underpowered to pin down the exact
  shape of the effect, only its rough magnitude and existence.

## Assessment against the plan's stop condition

The plan's instruction was to stop before Stage 1 if the effect is negligible
or only holds in an unrealistic parameter range. This came out ambiguous
rather than clearly one or the other:

- **Not clearly negligible**: several parameter combinations show a
  10–19%-of-ceiling gain, and the qualitative mechanism (hop decomposition
  enables riding out transient hop1 issues without abandoning the better
  path) is real and mechanistically sound, not an artifact.
- **Not clearly robust either**: the effect only exists in a specific
  switching-cost regime (neither too small nor too large relative to the
  hop1-bad-burst cost), the naive frictionless model gives exactly zero, and
  the realized magnitude is modest and noisy even in the favorable region of
  parameter space explored here.

This is a judgment call the plan explicitly reserved for you rather than
letting Stage 0 auto-decide. Two honest framings:

1. **Conditional go**: the mechanism is real and plausible for the actual
   isekai-pipe use case (path migration has a genuine, roughly known cost),
   so Stage 1 should focus specifically on calibrating `c_switch` against
   real QUIC path-validation/migration overhead and tightening the Monte
   Carlo estimate (more trajectories, variance reduction) rather than a
   broad re-sweep — if the realistic `c_switch` lands in the effective
   range found here, decomposition is worth prototyping; if not, stop.
2. **Stop here**: a single-digit-percent, noisy improvement over a narrow
   parameter band is a thin basis to justify Stage 1's added engineering
   cost (a car-side control channel propagating hop2 estimates, per the
   plan's §4.1), especially since Stage 1 can't make the *mechanism*
   stronger — only confirm whether Stage 0's idealized assumptions
   (Markov, independence) survive relaxation.

Recommend discussing which framing to take before starting Stage 1 work.

## Post-review fixes (2026-07-17) — see `FORMALIZATION_REVIEW.md` for the full critique

An external formalization review (Codex CLI + an independent Fable-model
agent) found two consequential defects in the model above, both now fixed:

**Fix A — `rho` was confounding correlation with hop2's own marginal
dynamics.** The old `joint_transition_matrix` mixed towards a regime where
hop2's next state is forced to copy hop1's, which silently overwrote hop2's
own `p_gb`/`p_bg` as `rho → 1` — the correlation sweep was secretly also
changing hop2's burst length. Replaced with a mixture of the independent
coupling and the *comonotone* (Fréchet upper-bound) coupling of the two
hops' marginal transition rows (`dmr/channels.py::_comonotone_coupling`),
which preserves each hop's exact marginal transition — hence burst length
and stationary bad probability — for every `rho`; verified numerically
(`hop*.stationary_bad_prob()` matches the joint stationary distribution's
implied marginal exactly at every `rho` tested). Finding 1's qualitative
shape (MI gap shrinks monotonically with `rho`) survives under the
corrected, now properly-isolated model.

**Fix B — observations were action-independent, which is physically wrong,
and turned out to matter far more than expected.** The original simulators
drew a hop1/hop2 loss observation every step regardless of which path was
actually carrying traffic. Fixed so hop1/hop2 losses are only observable
while path B carries traffic (`action == B`), or — in the warm-standby model
— while the standby is being kept warm (`action == A and m == WARM`).
Re-running after this fix produced a striking result:

- **`switching.py`'s model (no warm-standby option) collapsed to *exactly
  zero* decomposition value across the board**, not just at `c_switch=0`.
  Once the controller bails to path A, it can never observe path B again
  (no traffic, no standby probing), so its belief simply relaxes to the
  stationary distribution and it can never accumulate enough confidence to
  justify switching back — composite and decomposed observation are
  equally useless once you're blind. This is a *worse* form of the
  "stuck on path A" pathology than Finding 3 originally described (it was a
  precise numerical prediction from the Fable review, confirmed exactly).
- **The warm-standby model (`dmr/warm_standby.py`) recovers the effect**,
  because choosing to keep the standby warm now genuinely buys a fresh
  observation, not just a cheaper future switch. Under partial observability
  (`warm_standby_demo.py` Part 2, QMDP belief tracking, n_traj=400,
  n_steps=1500): composite-observation cost 0.07085 ± 0.00025 vs.
  hop-decomposed cost 0.06956 ± 0.00026 — a gap of **0.00129 ± 0.00036**
  (≈3.6 standard errors, i.e. a real, non-noise effect at this sample size).

**Net effect on the research question**: hop decomposition's value doesn't
just depend on a switching-cost/hysteresis mechanism (Finding 3) — under a
physically honest observation model, it depends on **also having some
mechanism to observe the relay path while not actively using it** (i.e.
warm-standby-as-probing). Without that, the "stuck on A" absorbing state
eliminates the effect entirely, regardless of switching cost. This sharpens
the research question (see the Artifact's §2, rewritten 2026-07-17): the
interesting object isn't "hop decomposition vs. switching cost" in isolation
but the three-way interaction between observation granularity, switching
cost, and standby-probing availability.

## Fix C — average-cost (RVI) criterion, replacing discounted planning

`value_iteration_switch`/`value_iteration_warm` optimized a discounted
(`gamma=0.95`) objective, while every evaluator (`induced_chain_avg_cost`,
all Monte Carlo simulators) scores long-run *average* cost — a criterion
mismatch Fable flagged (the discounted-optimal policy and its hysteresis
band location need not be average-cost optimal). Added
`average_cost_value_iteration_switch`/`_warm` (relative value iteration,
RVI) and switched all driver scripts to use them. Validated: RVI's computed
`g` matches `induced_chain_avg_cost` under its own policy exactly (to float
precision) in every scenario checked. In this project's specific parameter
regimes, the discounted-optimal policy happened to already coincide with
the average-cost-optimal one, so no earlier finding was invalidated — but
this is the mathematically correct criterion going forward, and matches
the restless-bandit/Whittle-index literature's standard convention.

**A genuine subtlety found while validating this**: at extreme `c_switch`
(large enough that path A becomes absorbing — never switches back to B),
`gap(active=B)` in `voi_analytic.py` *plateaus* rather than decaying back to
zero the way it does under discounted planning. This is not a bug: once
path A is absorbing, the one-time cost of an eventual bail-from-B — however
large — gets amortized to exactly zero over an infinite horizon, so
gain-optimality (plain average cost) is provably insensitive to it. This is
a known gap between gain optimality and bias/Blackwell optimality (Puterman,
*Markov Decision Processes*, ch. 8–10); it only bites at unrealistically
large `c_switch`, far outside the sweep's realistic range (0.05–0.2).

## Fix D — exact belief-grid POMDP solve, benchmarked against QMDP

QMDP's continuation term assumes the hidden channel state becomes fully
observed one step later — once warming genuinely buys information (Fix B),
this is exactly the assumption that breaks, and QMDP structurally cannot
value "pay to warm in order to observe." Implemented an exact-up-to-grid-
resolution solve (`dmr/beliefgrid.py` + `dmr/beliefgrid_warm.py`): value
iteration over a discretized belief simplex (all points with coordinates
that are multiples of 1/resolution), using `scipy.spatial.Delaunay` for
correct barycentric interpolation between grid points (an earlier hand-rolled
"Freudenthal triangulation" attempt had a real bug — caught by a direct
reconstruction-error check before it was used for anything — and was
replaced with the standard, tested Delaunay approach).

Validated three ways before trusting the result: (1) restricting the solve
to a single fixed action reproduces the fully-observed exact evaluator to
~1e-8; (2) Q-values/policies at near-certain belief points qualitatively
match the fully-observed optimal policy table; (3) `g` rises monotonically
with grid resolution (0.0638 → 0.0658 → 0.0663 at resolution 10/14/16),
exactly the expected direction for linear interpolation on a *concave*
cost-minimization value function (Lovejoy 1991: grid + linear interpolation
gives a systematic lower bound on the true optimal cost, tightening as
resolution increases) — a theoretically-predicted trend, not noise.

At resolution=14 (`beliefgrid_demo.py`): exact composite g=0.06579 vs.
decomposed g=0.06487, a gap of **0.00092** — compared to QMDP's Monte Carlo
estimate of 0.00124 ± 0.00036 from the same scenario. **The exact solve
confirms the same qualitative finding** (decomposition has real, positive
value) **at the same order of magnitude**, with the exact gap sitting
somewhat below QMDP's point estimate — i.e. QMDP appears to modestly
overestimate the decomposition-value gap here, consistent with (though not
dramatically so) the bias direction Fable predicted. Given resolution=14 is
still a lower bound not fully converged, this comparison should be read as
"same ballpark, QMDP not wildly wrong" rather than a precise bias
correction factor.

## Extension (2026-07-18): the (β1, β2) switching-curve derivation

`FORMALIZATION_REVIEW.md`'s §3 flagged a sharper deliverable than Fix D's noisy
Monte Carlo heatmaps: at `rho=0` (independent hops) with hop-decomposed observation,
the belief provably factors into two scalars `β1=P(hop1=Bad)`, `β2=P(hop2=Bad)`,
reducing the whole warm-standby POMDP to a 2D belief-MDP whose optimal policy is
characterized by two switching curves on the unit square. Full derivation, proofs, and
the numerically-probed gaps: **`THRESHOLD_PROOF.md`**. Code: `dmr/beliefgrid2d.py`
(the 2D solver, replacing `beliefgrid_warm.py`'s 3-simplex solve at `rho=0` — reaches a
tighter lower bound at far less compute) and `dmr/switching_curves.py` (curve
extraction + the always-warm sub-model where a full monotone-threshold theorem is
provable). Headline results:

- **Provable**: the always-warm-standby sub-model (standby forced warm every step)
  admits an exact clamp identity `Δ(β) = clamp(d(β), -c_switch, +c_switch)` that closes
  a monotone-POMDP induction completely — both switching curves are level sets of the
  *same* scalar field `d`, giving an exact hysteresis band `{|d(β)| < c_switch}`.
- **Not provable the same way**: the full 4-action model's action also picks the next
  `(active_path, warm_status)` context and has action-dependent observability, which
  breaks the plain Topkis argument (the value-of-information term between different
  observation regimes is hump-shaped in `β`, not monotone). Verified numerically
  instead: the *routing* decision's threshold structure held in every scenario tried
  (never proven, though — exactly 1 transition per slice, every context, every
  scenario). The *warm/cold* decision does break threshold structure, but not as a
  thin "band right at the boundary" (an early single-slice probe suggested that; a
  full-grid re-check corrected it) — it's a wide, `β2`-dominated **wedge**: while
  routing on the direct path, warming the relay (real information-gathering, since
  only the relay has hidden state in this model) is worth it whenever hop2 suspicion
  is below a cutoff (~0.46 in the scenario checked) *and* hop1 is still genuinely
  undecided, with that undecided range shrinking (non-monotonically — a real, small,
  unexplained wrinkle around `β2≈0.2`) as hop2 suspicion rises. Separately, warming
  the direct path *while already on the relay* is a different, cruder decision (pure
  switch-cost insurance, since the direct path has no hidden state to learn about),
  active over a much broader region. See `THRESHOLD_PROOF.md` §4 for the full
  breakdown, including the two-motive split this required.
- A figure (Artifact, built via the dataviz skill) plots both models' curves,
  hysteresis bands, the warm-wedge overlay, and one sample trajectory's hysteresis
  loop through belief space.

## Further theoretical deepening (2026-07-18)

A second planning-stage review (Codex CLI + an independent Fable-model agent) was
consulted before pursuing three requested extensions, staying entirely within
math/simulation (no real hardware experiments). See `THRESHOLD_PROOF.md` §6-§7 (and
its erratum in §3) for full derivations; headline results:

- **A real proof gap was found and fixed first.** Codex caught that the published
  proof of `d`'s monotonicity (the always-warm clamp theorem) contained an invalid
  step ("difference of two monotone functions is monotone" — false in general). The
  conclusion was unaffected (independently confirmed numerically before and after),
  but the proof was wrong until corrected via tracking `Δ_n = h_n(·,B)-h_n(·,A)`
  directly through the RVI induction (linearity of expectation at a *shared* next
  belief, not a difference of separately-monotone functions) — verified to hold at
  every one of 233 RVI iterations, not just the fixed point.
- **n-hop generalization** (`dmr/nhop.py`): both the belief factorization and the
  always-warm clamp theorem generalize cleanly to n hops for a *binary* routing
  choice (verified at n=3, factorization error 3.6e-16, zero monotonicity
  violations). Explicitly does NOT generalize to 3+ routing alternatives (candidate
  relay vehicles) — that's restless-bandit territory where switching costs are known
  to generally break Whittle indexability.
- **An exact (β1,β2,κ) chart and an O(ρ²) robustness theorem** (`κ := b_BB -
  β1·β2`, the deviation from product form): the predict-step recursion for `κ` is
  *exact* (not perturbative) and verified to 2.2e-16. Using standard MDP
  perturbation-sensitivity theory (Schweitzer/Cao potentials + the Milgrom-Segal
  envelope theorem), the ρ=0-optimal policy's performance loss when deployed at
  small ρ>0 is provably `O(ρ²)` — verified via a new noise-free exact policy
  evaluator (`beliefgrid_warm.evaluate_fixed_policy_belief_grid_warm`, since a first
  Monte Carlo attempt was underpowered, SE≈3e-4 against a ≈1e-4 effect): the
  suboptimality gap's excess growth over ρ∈[0,0.04] was ~100x smaller than the
  optimal value's own sensitivity to ρ.
- **The full model's routing-threshold monotonicity is FALSE in general — settled,
  not left open.** Per both reviews' recommendation to try to break it before
  attempting any proof, an adversarial search over 250 random (physically-plausible)
  parameter combinations found a genuine counterexample (228 violations at grid
  resolution 30), confirmed real (not a discretization artifact) by checking that
  `violation-magnitude × resolution` converges to a nonzero constant across
  resolutions 30/60/100/150. No general sufficient-condition proof was therefore
  attempted; the honest deliverable is the existing empirical certification for this
  project's realistic/calibrated parameter regime, now explicitly qualified as
  non-universal.

## Reproduce

```
uv run python run_stage0.py            # single-scenario walkthrough + sanity checks
uv run python sweep.py                 # parameter sweep, writes output/*.png, output/sweep_results.csv
uv run python voi_analytic.py          # exact (no Monte Carlo) Blackwell VoI-gap vs switching cost
uv run python warm_standby_demo.py     # adaptive warm-standby demo, oracle + partial-observability comparison
uv run python beliefgrid_demo.py       # exact belief-grid POMDP solve vs QMDP benchmark (~3-4 min)
uv run python beliefgrid2d_demo.py     # 2D (beta1,beta2) solver validation vs the simplex solve
uv run python switching_curves_demo.py # switching-curve derivation + proofs' numerical checks (THRESHOLD_PROOF.md)
uv run python nhop_demo.py              # n-hop generalization (THRESHOLD_PROOF.md §6), n=3
uv run python rho_perturbation_demo.py  # exact kappa recursion + O(rho^2) check (§7), ~2 min
uv run python adversarial_search_demo.py # counterexample to G1 routing monotonicity (§4), ~3 min
```
