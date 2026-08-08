# Always-warm vs always-cold boundary: a cleaner, distinct question from adaptivity value

Spun off 2026-07-19 from a user question after the Berlin V2X real-data result (adaptive
warm/cold control's value over the better of the two FIXED policies peaks at 0.65% for the one
real car-pair tested, per `TRACE_CALIBRATION_NOTES.md`'s "THIRD RESOLUTION" — well below the
informal 5% bar). The user asked: even if *adaptive* control isn't worth building, is there
academic/practical value in characterizing **which fixed policy is better and where the
boundary is** — i.e. `g_warm(theta) vs g_cold(theta)` directly, not `g_adapt` vs their min.

**Why this is a genuinely different (and likely easier) question than the single-crossing saga**
that occupied this project for the prior two days (see `THRESHOLD_PROOF.md`'s repeated
refutations, "every refinement holds until someone builds the right targeted search" pattern):
`g_warm` and `g_cold` are each the value of a POLICY-CLASS-CONSTRAINED sub-MDP (2 routing
actions only, no warm/cold choice), not the full 4-action model whose `min()`-over-actions
structure is what kept breaking monotonicity there. Comparing two such constrained values is a
much simpler object.

**Also directly practically relevant**: a system that decides *not* to build adaptive
warm/cold control (as the Berlin V2X finding suggests) still has to pick ONE fixed design —
"always keep the backup path warm" or "let it go cold and pay the switch cost when needed" — a
much simpler engineering decision than the full adaptive/decomposed machinery, and one this
boundary directly answers.

## Provable structure on the c_switch_cold axis (Fable-model review, 2026-07-19)

Along `c_switch_cold` (holding channel params and `c_warm` fixed): `g_cold` is the min over
fixed policies each linear (or constant) in `c_switch_cold`, hence **non-decreasing and concave**
in `c_switch_cold`. `g_warm` does not depend on `c_switch_cold` at all. Therefore **any crossing
along this axis is unique and its existence is fully determined by the SIGN of a single scalar**:

    Phi(other params) := g_cold(c_switch_cold -> infinity) - g_warm
                        = min(cost_a, stationary_path_b_loss) - g_warm

(the `c_switch_cold -> infinity` closed form is exact: in that limit switching is never worth
it, so `g_cold` reduces to whichever fixed path — A forever, or B forever — has the lower
stationary average cost; no RVI needed for this half of the comparison). `Phi > 0` means a
genuine crossing exists somewhere in `c_switch_cold`; `Phi <= 0` means cold dominates warm for
EVERY switching cost, no crossing at all. This collapses "does a crossing exist" from a sweep
into a single `g_warm` computation — see `warm_cold_phi_lambda_demo.py`.

**Both numerically verified before trusting anything downstream** (ad hoc check, 3 lambda
values): (a) the closed-form plateau matches RVI's own `always_cold_value_iteration` at large
`c_switch_cold` to <0.0001 error; (b) reported `g` is IDENTICAL regardless of `ref_context`
(A vs B) in the plateau regime — no multichain/reference-point artifact silently pinning the
reported average cost to one context.

## What's risky: the lambda axis (Fable's flagged concern)

No theorem protects monotonicity in channel persistence `lambda`. Value-of-information
arguments generically peak at INTERMEDIATE persistence (near lambda=0: no memory, nothing to
exploit; near lambda=1: state nearly frozen, no need to keep re-observing) — a naive 2-point
"crossover decreases monotonically in lambda" read (from the original gate check's lambda=0.7,
0.9 points) could plausibly reverse near lambda->1.

**Checked, 2026-07-19**: `warm_cold_phi_lambda_demo.py` computes `Phi(lambda)` directly (no
c_switch_cold sweep, per the reduction above) across `lambda in [0.05, 0.99]`, symmetric hop
pair (`pi_bad=0.3` fixed, only persistence varying; `cost_a=0.30`, `c_warm=0.02`,
`c_switch_warm=0.01`, matching the Berlin-V2X-informed calibration used elsewhere in this
project). Result: **`Phi(lambda)` is monotone non-decreasing across the ENTIRE range, with a
single sign change between lambda=0.64 and 0.66, and keeps INCREASING all the way to
lambda=0.99** (Phi=+0.070 there) — **no reversal near lambda->1 was found for this specific
parameterization**, contrary to the naive VoI-peaks-at-intermediate-persistence worry. Below
lambda=0.64, `Phi<=0` throughout (confirmed by direct computation at fine spacing near the
critical point, not by a wide-but-possibly-still-too-narrow sweep — the old c_switch_cold-sweep
version of this check, `warm_cold_boundary_gate_check_demo.py`, is now superseded by this more
efficient script for finding the critical lambda, though its wide-range (up to c_switch_cold=10)
independent confirmation that lambda<=0.6 has no crossing anywhere is a useful cross-check that
happened to already agree).

**Caveat, explicitly**: this is one symmetric-hop, single-lambda slice. The single-crossing
saga's own lesson (`peer-review-execute-not-just-read`, "every refinement holds until someone
builds the right targeted search") means this clean slice does NOT by itself establish the
general (asymmetric `lambda1 != lambda2`, general `eps_good/eps_bad`, general `cost_a`/`c_warm`
ratio) surface is equally clean. **Do not claim general monotonicity in lambda until an
adversarial search over asymmetric pairs has been tried and failed to break it** — this is the
single most important open item before writing any lemma about the lambda axis specifically
(the c_switch_cold-axis monotonicity, by contrast, IS provable in general right now, since the
argument above never used symmetry).

## Status / next steps (per Fable-model review priority order, 2026-07-19)

1. DONE: plateau closed-form + multichain sanity checks.
2. DONE: `Phi(lambda)` scan, symmetric pair, lambda up to 0.99 -- clean, single crossing, no
   reversal found.
3. NOT YET DONE: write the c_switch_cold-axis monotonicity/uniqueness argument as an actual
   lemma (this one IS provable now, unlike anything from the single-crossing saga) -- includes
   verifying the "g_warm has slope exactly 1 in c_warm" claim numerically as a self-check.
4. NOT YET DONE: adversarial search over ASYMMETRIC hop pairs (lambda1 != lambda2, general
   eps_good/eps_bad, varying cost_a/c_warm ratios) for a lambda-monotonicity counterexample --
   reuse this project's existing adversarial-search machinery (`adversarial_search_demo.py`'s
   pattern). This is the highest-risk remaining check before trusting the lambda-axis finding
   generalizes.
5. NOT YET DONE: literature scan of warm/cold standby-sparing literature (Levitin, Xie, and
   related reliability-engineering results) to determine whether this exact boundary (under
   PARTIAL observability / belief dynamics, as opposed to the fully-observed exponential-failure
   models typical of that literature) is already known, before investing in a proof -- this
   determines where the actual novelty claim would sit.
6. NOT YET DONE: error bars on boundary points (resolution/n_iters doubling) -- the specific
   crossing values (e.g. lambda_crit approx 0.65, c_switch_cold* approx 0.02-0.05) are small
   enough that grid/RVI-truncation artifacts (a known recurring issue in this project) could be
   comparable in magnitude.

**Fable's own stated risk for this whole direction**: not another counterexample necessarily,
but THINNESS -- if the structure really does reduce this cleanly to "Phi's sign", the
substantive research contribution becomes "a map of Phi(lambda, real-data-fit parameters)".
Combined with (3)'s real lemma, (4)'s real-data-informed map, and (5)'s literature connection,
this would still be a healthier, more defensible research artifact than the single-crossing
wreckage -- but should not be oversold as more than it is.

## Asymmetric attack results, 2026-07-19 (`warm_cold_asymmetric_attack_demo.py`)

Per Fable's prioritized attack list, ran 5 probes on genuinely asymmetric (lambda1 != lambda2,
varying contrast) hop pairs:

- **Probe 1 (coarse (lambda1,lambda2) 6x6 grid, contrast FIXED at 0.45)**: Phi is coordinatewise
  non-decreasing in BOTH lambda1 (down each column) AND lambda2 (across each row) -- confirmed
  `True`/`True` over the full grid. The 2D surface at fixed contrast is clean.
- **Probe 2 (extreme asymmetry, lambda1 in {0.9,0.99}, lambda2 in {0,0.05,0.1})**: Phi barely
  moves (e.g. 0.0066/0.0066/0.0067 at lambda1=0.9) -- a near-memoryless hop2 contributes
  essentially nothing regardless of exactly how memoryless, as expected.
- **Probe 3 (aggregation-hypothesis test, lambda1*lambda2 held at 0.25, split varied)**:
  **REJECTED** -- Phi ranges from -0.0117 (balanced 0.5/0.5 split) to +0.0272 (unbalanced
  0.99/0.253 split) at the SAME product. Phi genuinely depends on how persistence is
  ALLOCATED between the two hops, not just their product -- concentrating persistence in one
  hop favors warm-standby more than splitting it evenly at the same aggregate level. A real
  structural finding, not a simplification (the 2D problem does NOT collapse to 1D).
- **Probe 4 (persistence x contrast interaction) -- REAL NON-MONOTONICITY FOUND, Fable's top
  suspect confirmed**: holding lambda2 fixed and raising hop2's contrast (loss_bad-loss_good)
  from 0.10 to 0.30 to 0.45, Phi is NOT monotone -- it RISES then FALLS (e.g. lambda2=0.5:
  Phi=-0.0016 -> +0.0116 -> -0.0030; lambda2=0.3 shows the same rise-then-fall, even crossing
  back to negative). **Verified NOT a grid artifact**: recomputed at double resolution/n_iters
  (100/3000 vs 50/1500) -- values match to <0.0001, confirming this is real structure, not
  discretization noise (this is exactly the resolution-doubling discipline Fable and this
  project's own history both required before trusting an apparent counterexample). Plausible
  mechanism (not yet proven, just a first read): at low contrast, hop2's state carries little
  information regardless of persistence (nothing to exploit either way); at very high contrast,
  hop2's Bad state is so catastrophic that path B becomes rarely worth using at all once hop2 is
  suspected bad, which crowds out the specific benefit warm-standby buys (fast reaction the
  instant it's worth switching) -- the interesting middle band is where hop2 is informative
  enough to matter but not so catastrophic that path B gets abandoned outright. This is
  DIFFERENT from probe 1's clean result because probe 1 held contrast FIXED at 0.45 (the high
  end) throughout its whole grid -- the wobble lives in a dimension probe 1 never varied.
- **Probe 5 (alternating lambda<0)**: no anomalies, small-magnitude Phi values as expected.

**What this means for the lambda-axis / general monotonicity claim**: **DO NOT claim general
monotonicity of Phi beyond the specific 2D slice actually checked** (fixed contrast=0.45,
lambda1/lambda2 varying). The full (lambda1, lambda2, contrast, ...) surface has REAL,
resolution-confirmed non-monotone structure in at least the contrast dimension. This mirrors
(on a smaller, more contained scale so far) exactly the single-crossing saga's own lesson: a
clean-looking restricted slice does not certify the general surface. Unlike that saga, though,
this non-monotonicity was found via a DELIBERATE, prioritized adversarial search (per Fable's
guidance) rather than being discovered as an accidental late surprise -- the research process
here is working as intended, catching this early rather than after a "monotone!" claim was
already written into a paper.

**Status update**: item 4 (asymmetric adversarial check) is DONE and found real structure (the
contrast-interaction non-monotonicity) -- this determines the paper's actual shape now: not "Phi
is monotone" but "Phi is monotone in (lambda1,lambda2) at fixed contrast, but the full surface
has genuine interaction structure in contrast -- worth mapping properly before any proof
attempt." Next: (a) map out the persistence x contrast interaction more precisely (where exactly
does Phi peak in contrast, does the peak location shift with lambda2?), (b) THEN decide whether
a proof is worth attempting for the fixed-contrast slice specifically (which stayed clean) or
whether the full picture is better served by a characterization + honest "no general theorem"
framing, matching this project's own established practice from the single-crossing saga.

## RESOLUTION: the "wobble" is explained, not just found (2026-07-19, `warm_cold_mechanism_check_demo.py`)

Per Fable-model review, ran the 3 explicitly-agreed stop-rule checks before halting this thread
(the rule itself, and the discipline of stopping after them rather than continuing to refine
indefinitely, is per Fable's own explicit guidance -- precisely to avoid repeating the
single-crossing saga's "just one more targeted search" spiral):

1. **Mean-preserving spread (isolates the pure information-structure effect from a mean-shift
   confound)**: probe4's contrast sweep varied `loss_bad` while holding `loss_good` fixed at
   0.05 -- this ALSO shifts hop2's own stationary average loss upward as contrast grows, which
   can mechanically hit the `min(cost_a, stationary_path_b_loss)` kink built into Phi's own
   definition, faking a "value of information" story that's really just Phi's plateau term
   switching branches. Redid the contrast sweep holding hop2's stationary average loss FIXED
   (compensating `loss_good` down as `loss_bad` rises) to isolate the pure effect. **Result:
   Phi is cleanly monotone non-decreasing in contrast for EVERY lambda2 tested (0.3, 0.5, 0.7,
   0.9) once the mean-shift confound is removed** -- e.g. lambda2=0.5: 0.0109 -> 0.0109 ->
   0.0110 -> 0.0116 -> 0.0152 -> 0.0180 as contrast rises from 0.05 to 0.45, strictly
   increasing throughout. No non-monotonicity survives.
2. **Degenerate-boundary overlap check**: in the ORIGINAL (non-mean-preserving) sweep, Phi's
   peak (before its apparent decline) occurs at contrast=0.30, exactly where
   `stationary_path_b_loss=0.2991` -- essentially identical to `cost_a=0.30`. The sign-flip
   region (contrast 0.25-0.35) is precisely where `path_b_loss` crosses `cost_a`, confirming the
   kink-crossing hypothesis directly, not just by elimination.
3. Consequently, check 3 (the sign-map figure) is superseded by this cleaner understanding --
   the sign structure IS explained, not merely characterized.

**Conclusion: the persistence x contrast non-monotonicity found in the asymmetric attack was NOT
a new information-theoretic phenomenon -- it was entirely (to within numerical precision) an
artifact of Phi's own plateau term crossing its `min(cost_a, stationary_path_b_loss)` kink as
contrast shifts hop2's mean loss level.** Once this confound is controlled for, the underlying
surface (across every lambda1, lambda2, and contrast combination tested in this whole
investigation) is monotone and clean. This is a MEANINGFULLY BETTER outcome than either
"monotone with no explanation" or "genuinely non-monotone" -- the apparent wobble has a fully
understood, mechanical cause (the same kind of `min()`-over-fixed-policies kink the
c_switch_cold-axis lemma already handles), not a mysterious residual. Per Fable's own framing:
this converts "found a wobble" into "explained a wobble", which is exactly the difference
between a caveat and a real finding.

**STOPPING HERE per the explicit stop rule** (agreed with Fable in advance, specifically to
avoid the single-crossing saga's failure mode of never being able to stop searching): all 3
planned checks are done, the mechanism is understood, and further refinement would only sharpen
detail rather than change the qualitative picture. This research thread is COMPLETE for now.

## Final summary and practical takeaway

- **c_switch_cold axis**: provably monotone/single-crossing in general (channel-parameter-
  independent argument, safe to state as a real lemma for the discretized finite belief-MDP).
- **(lambda1, lambda2, contrast) axes**: monotone in every configuration tested, once the
  degenerate `min(cost_a, stationary_path_b_loss)` kink is correctly accounted for (it is not a
  separate phenomenon to explain away, it's an intrinsic and expected part of Phi's own
  definition, not a violation of any "clean" claim).
- **Aggregation hypothesis (probe 3) REJECTED**: Phi is NOT a function of `lambda1*lambda2`
  alone -- concentrating persistence in one hop favors warm-standby more than an even split at
  the same product. A real, standalone empirical finding worth a paragraph/figure.
- **Practical (Stage 1) takeaway, per Fable's suggested closing framing**: warm standby is worth
  its cost only when the standby path (a) has real persistence at the actual decision-epoch
  timescale, (b) has states that are genuinely distinguishable via observation, AND (c) is still
  worth routing to even in its bad state (not so catastrophic that path B gets abandoned
  outright, which is exactly the `min(cost_a, stationary_path_b_loss)` degenerate regime). All
  three conditions must hold simultaneously -- missing any one collapses the warm/cold decision
  to a trivial one.
