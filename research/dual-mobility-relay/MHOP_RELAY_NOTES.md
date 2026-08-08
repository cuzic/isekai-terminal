# M=2 candidate relay vehicles: a separate, exploratory research program

Everything else in this repo (`THRESHOLD_PROOF.md`, the paper) is scoped to a
**binary** routing choice: direct path `A` vs. one relay path `B`. This note
covers a distinct, exploratory extension — **M=2 candidate relay vehicles**
(3 routing alternatives: `A`, `R1`, `R2`) — kept separate because it is a
genuinely different mathematical structure (multi-regime optimal switching /
restless-bandit territory), not a straightforward generalization, and is not
part of the paper's claimed contribution.

## Why `dmr/nhop.py` doesn't apply

`nhop.py` generalizes the *n-hop* case (one relay path, `n` hops in series,
still a **binary** routing choice — see that module's docstring). The
always-warm clamp theorem's mechanism relies on `min` over exactly 2
branches collapsing to a single scalar field; `min` over 3+ branches does
not reduce the same way (Jun 2004; Glazebrook, Ruiz-Hernandez & Kirkbride
2006 — 3+-alternative switching is restless-bandit territory in general,
where switching costs are known to generally break Whittle indexability).

## Model (first cut, deliberately minimal — `dmr/mhop_relay.py`)

- Each candidate relay modeled as a single Gilbert-Elliott channel (not a
  multi-hop composite): belief is `(beta1, beta2) = P(R1=Bad), P(R2=Bad)`.
- **Always-warm-on-all-arms**: both relay channels observed every step
  regardless of which route carries traffic — mirrors the 2-arm always-warm
  sub-model's simplification, so there is no action-dependent-observability
  obstruction here by construction (no Gap-G1-style VoI hump). This is a
  deliberate first cut, not a claim that always-warm-on-all-arms is
  realistic.
- Uniform switching cost `c_switch` between any pair of distinct routes.
- Context = currently active route; solved via RVI on the same
  `(beta1,beta2)` unit-square grid as `beliefgrid2d.py`.

Validated (`mhop_relay_demo.py`) via a symmetric-scenario check (relay1 ==
relay2 implies the solution is exactly symmetric under `(beta1,beta2) ->
(beta2,beta1)` combined with `R1<->R2` swap — confirmed to machine precision,
max asymmetry 2.22e-16) and a degenerate-scenario check (an almost-always-bad
relay2 is chosen only ~4% of the belief-grid, as expected).

## Conjecture tested, and result

**Conjecture** (task #51): for each route `c`, is the "stay on `c`" region
monotone coordinatewise in `c`'s own belief coordinate — i.e., does a fixed
1D slice (varying one relay's belief, holding the other fixed) cross into
and out of the stay-region at most once, rather than flickering?

**Result: FALSE, but at roughly the same rate as the binary case — CORRECTED
2026-07-18.** The original count here (80/150 scenarios, 53%) was wrong: it
came from an off-by-one bug in `stay_region_monotone_check`
(`max(0, transitions - 1)` instead of `max(0, transitions - 2)`), caught by
an independent Fable-model review that actually re-ran the code rather than
trusting this note's numbers. **A stay-region that is a single clean
interval touching neither grid edge has exactly 2 transitions (one "enter",
one "exit") — that is the BENIGN, conjecture-satisfying case, not a
violation.** The old formula flagged every such ordinary interval as 1
violation; only 3+ transitions is genuine flickering. Re-checked (same
seed=2718, same 150 scenarios, fixed formula, independently re-run and
confirmed by the main session, not just the reviewing agent): of the
reported worst case, every flagged slice had exactly 2 transitions (zero
genuine flicker); **genuine-flicker prevalence across the 150-scenario
sweep is 9/150 (6%)** — the same order as, not dramatically higher than,
the binary-routing model's Gap G1 counterexample rate (12/250, 4.8%). The
"3+ alternatives makes monotonicity fail much more often" headline this
note originally led with is NOT supported by this search; the honest
finding is just "it's false here too, at a broadly comparable rate,"
which is a much weaker claim.

The conjecture is still FALSE (a genuine worst case with 3+ transitions per
slice exists and grows with resolution: 3 → 5 → 9 multi-transition
instances at resolution 30/60/100 for the corrected worst case below) — but
unlike Gap G1's `magnitude × resolution → nonzero constant` check, this is a
*count* of flagged slices, not a magnitude, and no equivalent artifact-vs-
real convergence diagnostic has been established for it yet; growth with
resolution here is suggestive, not yet proven non-artifactual the way G1's
was.

Corrected worst-case parameters (genuine 3+-transition flicker): `relay1` —
`p_gb=0.0188, p_bg=0.1108, eps_good=0.0131, eps_bad=0.4415`; `relay2` —
`p_gb=0.0366, p_bg=0.0359, eps_good=0.0480, eps_bad=0.5317`; `cost_a=0.2899`,
`c_switch=0.2367`. (The old note's "worst case" parameters, reproduced below
for the record, turned out to be a benign-interval case, not a flicker case:
`relay1` — `p_gb=0.0053, p_bg=0.0658, eps_good=0.0257, eps_bad=0.8086`;
`relay2` — `p_gb=0.0129, p_bg=0.0555, eps_good=0.0316, eps_bad=0.1554`;
`cost_a=0.2663`, `c_switch=0.3366`.)

## Literature this connects to (per task #51, regardless of outcome)

- Jun, T. (2004); Glazebrook, K., Ruiz-Hernandez, D. & Kirkbride, C. (2006)
  — restless-bandit indexability under switching costs, already cited in
  `THRESHOLD_PROOF.md` section 6/9 as the reason 3+-alternative routing is
  out of scope for the main paper's claims.
- Djehiche, B., Hamadene, S. & Popier, A. (2009-2010) — multi-regime
  optimal switching with more than 2 regimes; the natural continuous-time
  analogue of this discrete-time, 3-route belief-MDP.
- Pham, H., Ly Vath, V. & Zhou, X.Y. (2009) — optimal switching over
  multiple regimes, viscosity-solution characterization.
- Hu, Y. & Tang, S. (2010) — multi-dimensional BSDEs and optimal switching
  with more than 2 regimes.

## Status

Falsified as conjectured (at ~6% prevalence, corrected from the erroneous
53% figure above); no proof of any sufficient condition attempted (same
"don't build on a false universal claim" discipline as Gap G1). This is
flagged as a separate, unproven research direction — not integrated into
`dual_mobility_relay_paper.html`'s claims, per task #51/#53's explicit
scoping (the paper's positioning stays on the already-identified unoccupied
axis: Blackwell garbling layered on switching-cost/hysteresis/probing
control structure for the **binary**-routing case). Given the corrected,
much weaker prevalence contrast with Gap G1, a naive "extend to M=3/M=4 and
plot violation-rate vs. choice-cardinality" follow-up (previously
considered as a next step) is no longer well-motivated as stated — see the
2026-07-18 consultation note below for a better-scoped alternative.

## 2026-07-18 follow-up consultation (Codex CLI + independent Fable-model review)

Consulted both on 3 candidate next directions for the main (binary-routing)
research thread; Fable's review is what caught this note's counting bug
above by actually re-running the code rather than trusting the reported
numbers — a concrete instance of why independent code-level verification
matters more than a purely strategic/literature-based review. Summary of
the consultation, for the main thread (not this M=2 side project):

- **Prove "policy stays a threshold" via single-crossing (Milgrom & Shannon
  1994) rather than full monotonicity of `d`**: both reviewers rate this as
  the most promising next step, but only as a "quick falsification gate
  first" (a directed adversarial search specifically for a POLICY-level
  multi-crossing, not just a field-level one — 4-12h), before committing to
  a proof attempt (20-80h, uncertain odds; the aggregation step in the
  Bellman backup is exactly where single-crossing properties are known to
  be fragile — Quah & Strulovici 2012 on aggregating single-crossing
  properties is the key reference). Fable additionally ran a cheap
  cost_a-continuation probe on the trial-90 witness and found the
  non-monotone dip's policy-multi-crossing risk vanishes well before a
  perturbed decision boundary could reach the dip — mild encouraging
  evidence, not a proof.
- **Turn the validated invariant into an analytic sufficient condition**:
  both reviewers recommend NOT pursuing this — the existing gate check
  (#58) already showed the natural bounding approach fails by ~18x at the
  calibrated scenario, and the invariant's own direction (violators skew
  toward LOWER hop-persistence) contradicts what standard mixing/contraction
  bounds would predict, meaning the mechanism isn't understood well enough
  yet to prove an inequality in a specific direction.
- **Extend M=2 to M=3/M=4 and measure a violation-prevalence-vs-cardinality
  curve**: Codex rated this the second-most-valuable "reliable new result";
  Fable, after finding this note's counting bug, argues the motivating
  premise (M=2 much worse than binary) no longer holds, and recommends
  reframing as a geometric study of WHERE genuine flicker occurs (e.g., near
  triple points where three stay regions meet) rather than a prevalence
  curve, if pursued at all.
- **4th directions suggested**: Fable proposed a cheap (~4-8h) 1D single-
  relay reduction of the always-cold model as a decisive, nearly-free
  precursor to the single-crossing proof attempt. Codex proposed reframing
  any "provable sufficient condition" ambition as a certified numerical
  theorem for the paper's specific calibrated parameter box (interval/grid
  refinement) rather than chasing an unbounded analytic inequality.

## Task #68: `stay_region_monotone_check` still isn't a strict single-interval check

Codex's task-list review (2026-07-18) caught a further subtlety in the just-fixed
transition-counting logic: `max(0, transitions - 2)` correctly detects 3+-transition
"flicker", but a slice like `[1,0,1]` (stay at both edges, a gap in the middle — two
DISCONNECTED components) also has exactly 2 transitions, identical to a single clean
interval, so the transition count alone cannot tell them apart. Added
`dmr.mhop_relay.stay_region_connected_components_check`, which counts contiguous
`True`-runs directly (any slice with more than 1 run is a genuine disconnection,
regardless of transition count) as a strict complement to the flicker check. Validated
on a synthetic `[1,0,1]` case (correctly detected as 1 disconnected column, 2 runs) and
against the corrected worst-case witness above, both independently by this session and
by a Codex CLI review that ran its own check rather than trusting the report: at
resolution 100, context A's `beta1`-columns metric happens to agree exactly
(`n_disconnected_columns_beta1=5` matches `n_multi_transition_columns_beta1=5` there),
**but the two checks' OVERALL totals across all contexts/axes do NOT match**
(monotone-check total 9 vs. connected-components total 7 at resolution 100, and 3 vs. 2
at resolution 30, 5 vs. 4 at resolution 60) — they count structurally different things
(excess-transitions vs. disconnected-slice count) and should not be expected to agree in
general, only in this one coincidental slice. Future analyses (e.g. task #66's
geometric re-analysis) should use the stricter connected-components check, not just the
flicker check, to avoid missing a `[1,0,1]`-style disconnection a transition count alone
cannot see.

## Task #66: geometric location of genuine disconnections — NOT near triple points

Fable's 2026-07-18 hypothesis was that genuine stay-region disconnections (task #68's
strict connected-components sense) would cluster near "triple points" — beliefs where
all 3 routes (A, R1, R2) are near-equally competitive. Checked directly
(`mhop_relay_geometry_demo.py`) on the corrected worst-case witness (resolution 100):
5 genuine disconnections were found (all in context `A`'s `beta1`-direction columns,
none in R1/R2 or the `beta2`-direction), and the point of closest 3-way cost approach
(the best triple-point candidate, spread ≈6.1e-3) sits at `(β1,β2)≈(0.81, 0.62)`.
**The 5 disconnected slices sit at `β2≈0.33–0.37` — a mean distance of 0.27 from the
triple point's matching coordinate, on a grid spanning `[0,1]`.** This does NOT support
the "flicker occurs near triple points" hypothesis on this witness — the disconnection
geometry looks more diffuse, tied to something else (plausibly the same route-A-specific
mechanism `stay_region_monotone_check` already showed was concentrated entirely in
context A, not evenly spread across all 3 routes' stay-regions). Not investigated
further given this task's own low-priority framing — reported as tested-and-not-
confirmed rather than silently dropped.

Reproduce: `uv run python mhop_relay_demo.py` (solver validation, ~10s),
`uv run python mhop_relay_search_demo.py` (falsification search, ~2-5 min),
`uv run python mhop_relay_geometry_demo.py` (triple-point geometry check, ~2 min).

## R3 (2026-07-19): the missing artifact-vs-real gate now built and PASSED — the M=2 disconnection is real

An independent Codex+Fable consultation (on whether an M≥3 extension, task #67, is worth
attempting) both flagged the same gap: unlike Gap G1 (which has a `violation-magnitude ×
resolution` convergence check confirming it's real, not a discretization artifact — see
`THRESHOLD_PROOF.md` §4), **the M=2 stay-region disconnection finding (task #68's corrected
6% prevalence) had never been put through an analogous check.** Fable proposed the missing
diagnostic: measure the total CONTINUOUS-coordinate width of the internal gap between a
disconnected stay-region's components (not a raw grid-cell count, which trivially grows with
resolution even for a single-cell artifact), across increasing resolution — a real geometric
gap converges to a fixed positive width; an artifact shrinks like ~1/resolution toward 0.

Built (`mhop_relay_gap_convergence_demo.py`) and run on the same corrected worst-case witness
as task #66, at resolutions 30/60/100/150/200/300. **Result: cleanly convergent, not an
artifact.** All 3 contexts' max gap width is stable to within a 1.00 max/min ratio across the
finest 3 resolutions tested (context A: ≈0.706-0.717, context R1: ≈1.003-1.017, context R2:
≈0.973-1.000 — note context R1/R2's "width" exceeds 1.0 because it sums gaps across BOTH the
beta1-direction and beta2-direction slice families, not a single slice's own span). **This
gates IN the M=2 finding as real**: the 6% genuine-flicker prevalence (task #68) and the
disconnection phenomenon itself can be trusted as describing an actual feature of the
continuous model, not a discretization fluke.

This does NOT by itself revive task #67 (M≥3 extension) — Codex's and Fable's other objections
(the "near triple points" motivating hypothesis already tested and not confirmed per task #66
above; exact grid RVI's `resolution^M` blowup; M≥3 needing a genuinely different sparse/adaptive
numerical method, not a small generalization of the current exact grid solver) still stand
unaddressed. What this DOES establish is that the M=2 phenomenon itself is worth taking
seriously as a real mechanism to eventually understand (not dismiss as noise) if anyone returns
to this thread, and that any future M≥3 attempt should build the same width-convergence check
from the start rather than reporting raw prevalence/count numbers alone.

Reproduce: `uv run python mhop_relay_gap_convergence_demo.py` (~3-5 min, resolutions up to 300).
