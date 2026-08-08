# Pure-Gilbert analytical treatment of the warm/cold boundary — handoff notes

Spun off 2026-07-19 from a user question ("could the warm/cold boundary be answered more
quantitatively/analytically?") following `WARM_COLD_BOUNDARY_NOTES.md`'s numerical
characterization. This note records what was established and validated THIS session, and exactly
what to pick up next time — per an explicit Fable-model recommendation to stop the mechanical
symbolic-derivation work here (end of a long session is exactly when cycle-accounting bugs get
introduced) and hand off cleanly instead.

## The core result (established and numerically validated this session)

Under **pure-Gilbert channels** (`eps_good=0, eps_bad=1` exactly — loss is a deterministic
function of hidden state, not the general partial-observation case), the always-warm sub-model's
continuous-belief POMDP (`switching_curves.always_warm_value_iteration`) provably reduces to a
**finite, fully-observed 8-state average-cost MDP** ((channel joint state in {GG,GB,BG,BB}) x
(current context in {A,B}), 2 actions), exactly solvable by policy iteration with NO grid/RVI
truncation error at all. See `pure_gilbert_finite_mdp_demo.py`.

**Why**: "always warm" means BOTH hops are observed every step (active hop via live traffic,
standby hop via the warm probe) -- see `_continuation_always_warm` in `switching_curves.py`,
confirmed to have no "if observable" branch, unlike the always-cold continuation. With
`eps_good=0, eps_bad=1`, an observation deterministically reveals the true hidden state, so the
LAST OBSERVED joint state is a sufficient statistic for the whole belief.

**A timing bug found and fixed while building this (worth remembering, cost real debugging
time)**: the belief does NOT stay at the 4 corners `{0,1}x{0,1}` -- it stays at the 4 PREDICTIVE
points `{p_gb1, 1-p_bg1} x {p_gb2, 1-p_bg2}` (one Markov-transition step past the last exact
observation), because the decision for THIS step must be made using the predictive distribution
of the CURRENT (not-yet-observed) state given the LAST observation -- decisions happen before
this step's own observation resolves it. Concretely: the immediate cost of routing to B in
last-observed-state `c` is `E_{c'~T_channel[c,:]}[path_b_loss(c')]` (a one-step-ahead
expectation), NOT `path_b_loss[c]` directly (that would wrongly assume the CURRENT state is
already known before acting). Both the author and an independent Fable-model review found this
same bug independently while cross-validating against the continuous solver -- a real,
non-obvious modeling subtlety, not a coding slip, worth flagging prominently for any future work
on this reduction.

**Validated**: cross-checked the finite-MDP's exact `g` against
`always_warm_value_iteration`'s RVI solution at the eps=0/1 boundary across
lambda in {0.3, 0.5, 0.7, 0.9} (symmetric hop pair, pi_bad=0.3, cost_a=0.30, c_warm=0.02,
c_switch_warm=0.01): agreement to 1e-10-1e-17 (machine precision) in every case. The optimal
policy found is clean and interpretable: **route to B iff the last-observed joint state was GG**
(context-independent), except in one parameter regime where "always A" is exactly optimal
(lambda=0.3 in the test above) -- both are legitimate, not a discretization artifact.

## What is NOT yet done (the honest gap)

No explicit closed-form formula for `g_warm(p_gb1,p_bg1,p_gb2,p_bg2,cost_a,c_warm,c_switch_warm)`
has been written down yet -- only the finite-MDP reduction + numerical policy iteration. Per
Fable-model review: **this is fine to leave for next time.** The reduction itself (existence of
an exact finite algorithm, no continuous-POMDP approximation needed) is already the substantive
"more analytical" answer the user asked for; explicit rational-function formulas follow
mechanically from here and are secondary "decoration," not the core result.

## Policy enumeration (done, but NOT yet canonicalized -- do this first before any symbolic work)

A broad random sweep (p_gb1,p_bg1,p_gb2,p_bg2 in [0.005,0.9], cost_a in [0.02,1.0], c_warm in
[0.0005,0.2], c_switch_warm in [0.0005,0.15], n=1500 trials, see
`pure_gilbert_policy_enumeration_demo.py`) found **52 distinct raw policy vectors**. Per
Fable-model review, this count is almost certainly inflated by two things that must be corrected
BEFORE any symbolic work is attempted:

1. **Unreachable-state noise**: two policies that differ only in what they'd do from a
   transient/unreachable state (never visited under either policy's own induced recurrent class)
   have the SAME `g` and should be counted as the same policy. Canonicalize by actual `g`-value
   equivalence (or by the policy restricted to each candidate's own recurrent class), not raw
   8-vector equality.
2. **Near-boundary ties**: a sample point landing near a cell boundary can return either
   neighboring policy depending on floating-point noise during policy iteration. For each
   low-count policy found, check the `g`-gap against the locally-competing policy at that same
   parameter point; gaps below ~1e-9 are boundary noise, not a genuine 53rd cell.

After both corrections, expect substantially fewer than 52 real cells (Fable's estimate: possibly
15-25, still more than the "5-10" originally guessed, but each is a genuine, finite,
semi-algebraic cell -- NOT an instance of the single-crossing saga's unbounded-complexity failure
mode, since the total policy count is hard-capped at 2^8=256 regardless of how hard anyone looks).

**Scoping rule for eventual symbolic work** (Fable's recommendation): don't aim for one giant
7-variable `Phi=0` algebraic monster covering all cells -- that would be unreadable and add no
insight. Instead: (i) state the reduction theorem itself, (ii) derive explicit closed forms only
for READABLE special cases (symmetric hop pair; the always-A/always-B degenerate boundary), (iii)
leave the general case as "exactly computable via the finite algorithm," not further symbolic
expansion. Restrict any "which cells matter" question to the parameter region this project
actually cares about (real-data-fit-adjacent, lambda>0, realistic cost_a/c_warm ratios), plus
whichever cells are adjacent to the Phi=0 boundary regardless of their occupancy fraction in a
uniform random sweep (a rare-but-boundary-adjacent cell matters; a common-but-far-from-boundary
cell doesn't).

## The two trivial policies' g -- record now, no sympy needed (anchors for future plateau/mask checks)

- **Always A** (never touch path B): `g = cost_a + c_warm` exactly (constant cost every step,
  switch cost never paid once context settles on A). This is the SAME closed form already used
  as the `c_switch_cold -> infinity` plateau anchor in `WARM_COLD_BOUNDARY_NOTES.md`.
- **Always B** (never touch path A): `g = stationary_path_b_loss + c_warm` exactly, where
  `stationary_path_b_loss` is the joint-stationary-distribution-weighted average of
  `channels.path_b_loss_prob` (same quantity already computed throughout this project's gain
  sweeps).

These two are NOT rational functions requiring derivation -- they're immediate from the cost
structure and already used elsewhere in this project. Use them to sanity-check any future
symbolic derivation's degenerate limits (e.g. as `c_switch_warm -> infinity`, the optimal policy
should converge to whichever of these two is cheaper) and to identify/mask the "degenerate"
region (per the multichain caveat below) directly rather than trusting a generic solver there.

## Cold side: renewal structure CONFIRMED, but it's semi-Markov, not a single ratio (correction to the original hypothesis)

The original hypothesis (a single renewal-reward cycle: "ride B until confirmed bad, retreat to
A for a FIXED n* steps, return") is **numerically confirmed to be qualitatively real** -- but
Fable's own more careful re-read of the confirming data caught that it's **more structured than
one uniform cycle**: the optimal parking duration `n*` genuinely depends on WHICH joint state
triggered the retreat (verified by direct trajectory tracing against
`switching_curves.always_cold_value_iteration`'s solved policy, symmetric hop pair pi_bad=0.1,
lambda1=0.5, lambda2=0.7, cost_a=0.30, c_switch_cold=0.10, resolution=50):

- Entering the cold spell from state **BB** (both hops just went Bad): parks for **5** steps
  before returning to B (traced via the deterministic `predict_scalar` recursion from beta1=1,
  beta2=1 -- belief decays 1.0->0.55->0.325->0.2125->0.1563->0.1281, switching triggers at
  t=5).
- Entering from **BG** (hop1 Bad, hop2 Good): parks for **3** steps (beta1=1.0->0.55->0.325->
  0.2125, switch at t=3).
- Entering from **GB** (hop1 Good, hop2 Bad): parks for **4** steps (beta2=1.0->0.73->0.541->
  0.4087->0.3161, switch at t=4).

A separate parameter regime (symmetric pair, pi_bad=0.3, lambda1=0.5, lambda2=0.7, cost_a=0.30,
c_switch_cold=0.10) gives `n*=infinity` (never returns) from the BB-entry trajectory, because the
stationary point (0.3,0.3) sits just outside the switch-back region -- this is the ALREADY-KNOWN
plateau/degenerate case from `WARM_COLD_BOUNDARY_NOTES.md`, not a new finding, but a useful
cross-check that the two independently-developed pieces of this analysis agree.

**Correction to record explicitly**: because `n*` differs by entry state (5 vs 3 vs 4, not one
common value), `g_cold` for a policy in this family is NOT simply `(B-phase cost + A-phase
cost)/(B-phase length + A-phase length)` for a single averaged cycle -- it is a **semi-Markov
average over an embedded Markov chain on the 3 distinct exit states** (each "cycle" = ride B
until one of {BB,BG,GB} triggers retreat, park for that state's own fixed `n*(state)`, return to
B, observe the actual state on return -- which is itself random, drawn from the transition kernel
`n*(state)` steps after the known parking-start state -- and continue). The renewal-reward ratio
formula still applies in principle (this is a completely standard semi-Markov/Markov-renewal
average-cost construction), but the cycle accounting has several real bookkeeping traps to get
right, explicitly flagged by Fable as best done with a fresh mind rather than at the end of an
already-long session:
- Which DIRECTION `c_switch_cold` gets charged on (A->B return vs B->A retreat) and whether it's
  charged once or twice per cycle.
- What happens if the state observed upon RETURN to B is immediately bad again (does the agent
  retreat again right away, extending effectively into a different/longer cycle, or does the
  policy still commit to riding out at least one step on B first)?
- The distribution over which of the 3 retreat-triggering states you land in upon return is NOT
  the stationary distribution -- it is the transition kernel applied `n*(entry state)` times
  starting from a KNOWN entry state, which differs per entry state and must be computed exactly,
  not approximated.

**Do not attempt the semi-Markov cycle-accounting derivation without addressing these three
points explicitly** -- per Fable's assessment, getting any one wrong would produce a plausible-
looking but incorrect closed form, exactly the kind of mistake this project's own established
practice (verify by re-execution, not by re-reading) exists to catch, so any attempt should be
cross-validated numerically against `always_cold_value_iteration` before being trusted, the same
way the warm-side reduction was.

## Known implementation gotcha: multichain degeneracy in policy iteration

`pure_gilbert_finite_mdp_demo.py`'s `solve_average_cost_policy_iteration` hit a genuine singular-
matrix error when policy iteration passes through a degenerate intermediate policy (e.g. "always
A"), because that policy makes `context=B` states unreachable/transient, breaking the unichain
assumption the Poisson-equation linear solve relies on. **Current fix**: fall back to
`np.linalg.lstsq` (minimum-norm least-squares) when `np.linalg.solve` hits a singular matrix.
This happened to return the correct converged `g` in every case tested here (cross-validated
against the continuous solver), but Fable explicitly cautions this is NOT guaranteed in general
-- a minimum-norm solution has no average-cost-correctness guarantee for a genuinely multichain
system. **Safer fix for next time**: detect degenerate policies directly (constant-action
policies collapse to the two closed forms recorded above) and assign their `g` directly rather
than running them through generic policy iteration at all; only trust `lstsq`-assisted
convergence within the parameter range already cross-validated against the continuous solver
(everything reported in this note qualifies; new parameter regions should be re-validated before
trusting).

## Regression-test benchmarks for next session (pin these numbers)

- Cross-validation agreement: symmetric hop pair, pi_bad=0.3, cost_a=0.30, c_warm=0.02,
  c_switch_warm=0.01, lambda in {0.3,0.5,0.7,0.9} -> finite-MDP `g` matches
  `always_warm_value_iteration` (resolution=80, n_iters=3000) to `<1e-9` in every case (exact
  values: 0.32000000, 0.31169450, 0.25891562, 0.20253818 respectively). Any future change to
  either the finite-MDP builder or the continuous solver should be re-checked against these.
- Renewal-duration benchmarks: pi_bad=0.1, lambda1=0.5, lambda2=0.7, cost_a=0.30,
  c_switch_cold=0.10, resolution=50 -> `n*(BB)=5, n*(BG)=3, n*(GB)=4`. pi_bad=0.3 (same
  lambda1/lambda2/cost_a/c_switch_cold) -> `n*(BB)=infinity` (never returns from this specific
  trajectory within 15 steps traced).
- Policy enumeration: 1500-trial sweep as specified above -> 52 raw distinct policy vectors,
  top-2 (always-A, always-B) account for 68% (839+187 of 1500) -- NOT yet canonicalized per the
  two corrections above; re-run canonicalization before citing "52" anywhere durable.

## UPDATE 2026-07-19 (later same day): warm-side symbolic derivation DONE and triple-validated; cold-side symbolic attempt has a real, still-unresolved bug

Per user request, actually did the sympy work this time, consulting an Opus-model advisor
throughout (`opus-symbolic-advisor`). See `pure_gilbert_symbolic_warm_demo.py`.

### Warm side: COMPLETE, triple-validated

Reparametrized via `(pi_b, lambda)` (advisor's recommendation: `pi_b = p_gb/(p_gb+p_bg)`,
`lambda = 1-p_gb-p_bg`, so `p_gb = pi_b*(1-lambda)`, `p_bg = (1-pi_b)*(1-lambda)`). Built the
channel stationary distribution and one-step kernel symbolically from scratch (NOT copying the
advisor's formulas), and derived, for the SYMMETRIC hop pair:

- `g(always-A) = cost_a + c_warm`
- `g(always-B) = c_warm + pi_b*(2-pi_b)`
- `g(route B iff last-observed=GG) = c_warm + a^2*(1-q_G^2)*(1+2*c_switch_warm) + (1-a^2)*cost_a`
  where `a = 1-pi_b`, `q_G = 1-pi_b*(1-lambda)`.

**Validated three ways**: (1) symbolic flow-balance check `P(GG->not GG) == P(not GG->GG)`
confirmed to be exactly 0 by sympy simplification (not assumed); (2) matches the advisor's
independently hand-derived formulas exactly (`sp.simplify(mine - advisors) == 0` for both
non-trivial cases); (3) matches the actual numerical solver (`pure_gilbert_finite_mdp_demo.py`'s
exact policy iteration) at 4 different random parameter points to 1e-16-1e-17 (machine
precision), with the solver's own chosen policy correctly identified at each point (all 4 cases
picked "route B iff GG" here, confirming this is a real, commonly-occurring, non-degenerate
policy, not a corner case).

**This can be cited as solid, done work.** The formulas above are correct and safe to write into
any future paper/notes section on this topic.

### Cold side: structural hypothesis MC-confirmed, but the EXACT semi-Markov formula still has a bug (NOT resolved -- do not trust the specific numeric formula yet)

Attempted the semi-Markov renewal-reward construction per the advisor's detailed plan (absorbing
fundamental matrix for the B-ride phase, embedded chain over entry-types {H="one hop bad" (GB/BG
merged under symmetry), BB="both hops bad"}, left-eigenvector stationary weights, T^n* applied
from the known entry state for the return-state distribution). Three successive attempts, each
fixing a real bug found via cross-validation against the continuous solver
(`always_cold_value_iteration`, symmetric pair `pi_b=0.1, lambda=0.5, cost_a=0.30,
c_switch_cold=0.10`, true `g_cold=0.17406932`):

1. First renewal-ratio attempt (single-step transitions used for the embedded chain instead of
   the correct `T^n*`): `g=0.2544` (way off, diff=0.080).
2. Rebuilt as an explicit finite Markov chain (states = `ride(c)` for last-observed channel state
   c, `park(entry_type, k)` for k=n_e..1 remaining park steps) but used the WRONG cost formula
   for the "return to B" transition (used the generic 1-step `predictive_loss[e]` instead of the
   n_e-step-decayed `(T^{n_e} @ path_b_loss)[e]`): `g=0.2404` (diff=0.036).
3. Fixed the return-step cost to use `T^{n_e}` (confirmed independently that this matrix-power
   calculation itself is correct: cross-checked `T^3[BG,:] @ path_b_loss` against the scalar
   `predict_scalar` recursion applied 3 times from `(beta1,beta2)=(1,0)` -- both give
   `0.28140625` exactly, ruling out a matrix-power bug) and removed a double-counted
   "instantaneous bad observation" state: `g=0.1851` (diff=0.011 -- much closer, but NOT machine
   precision, and a residual, unidentified bug remains).

**Ruled out as the cause of the remaining 0.011 gap**: (a) grid-resolution error in the
numerically-traced `n*` values -- re-traced `n*(BB)=4, n*(H)=3` at resolution 60, 120, AND 200,
identical every time, and swept ALL integer `(n_H, n_BB)` combinations in `[1,6]x[1,6]` -- (3,4)
is still the closest, ruling out an off-by-one; (b) asymmetry in choosing GB vs BG as the "H"
representative state -- verified `T[GB,:]` and `T[BG,:]` are exact mirror images (swap indices
1<->2, matches per the symmetric-hop construction), so this can't matter; (c) the policy
structure itself being wrong (immediate retreat on any non-GG observation) -- a **direct Monte
Carlo simulation of literally this policy** (3M steps, tracking the TRUE channel state directly,
no belief abstraction at all) gives `g≈0.1774` (stderr estimate ~0.0006, though likely an
underestimate of the true stderr given within-cycle autocorrelation), close to the true 0.17407
and NOTABLY CLOSER than the deterministic finite-chain calculation's 0.1851 -- **this is itself
informative**: since the MC estimate has no systematic bias (it directly implements the stated
policy with no analytical shortcuts) and lands closer to the true value than the hand-built
finite chain, the remaining bug is almost certainly in the finite-chain/renewal-ratio
BOOKKEEPING specifically (most likely in the switch-cost attribution or in exactly how the
return-transition's outcome-state is used to re-enter the ride/park cycle), not in the
underlying "fixed-n* renewal" structural hypothesis, which now has THREE independent forms of
support (the original policy-grid trajectory trace, the resolution-stability check, and this
Monte Carlo simulation).

**Do not present a specific closed-form g_cold formula as validated** until this residual bug is
found -- the qualitative renewal structure is solid, but the exact arithmetic is not yet right.
**Next-session starting point**: compare the Monte Carlo simulation's exact step-by-step
bookkeeping (see the `total_cost += ...` lines in the MC script embedded in this session's
transcript, or re-derive similarly) against the finite-chain's transition/cost definitions
LINE BY LINE -- the MC code is simpler (tracks true state directly, no belief abstraction) and
is the more trustworthy reference at this point; the bug is somewhere in translating that
correct-by-construction simulation logic into the stationary-distribution-based exact
calculation (most likely: a mismatch in exactly which step "owns" the switch-cost charge, or a
subtlety in whether the return step's outcome should be drawn from `T^{n_e}` or a related but
distinct quantity -- re-examine whether "current belief at decision time" and "this step's
directly realized/observed state" are being conflated somewhere, since that distinction is
exactly what took 3 rounds to get partially right on the warm side too).

## RESOLVED, 2026-07-19 (same day, continued): the bug was a return-step exponent AND park-count coupling

Per the Opus-model advisor's specific bug-hunt guidance (a decisive `c_switch_cold=0` isolation
experiment, plus a concrete hypothesis about the return-step's decay exponent), found and fixed
the actual bug:

**The advisor's `c_switch_cold=0` isolation experiment ran first**: with switch cost zeroed out,
the model-vs-solver diff was STILL 0.0127 (not reduced), ruling out switch-cost mis-attribution
(the advisor's "Suspect B") as the dominant cause and confirming the return-step's decay exponent
(the advisor's "Suspect A": the return step should use `T^{n_e+1}`, not `T^{n_e}`) was the real
culprit. Fixing the exponent alone (keeping park-step count and switch charges as before) shrank
the diff from 0.011 to 0.0036 -- a big improvement but not yet exact.

**The final piece**: re-sweeping `(n_H, n_BB)` with the corrected `T^{n_e+1}` exponent found the
EXACT match was at `(n_H=2, n_BB=3)`, not `(3,4)` as originally traced -- i.e., the true fix
wasn't just "use exponent n_e+1 instead of n_e" while keeping the same park-step count `n_e`;
it's that the ORIGINAL numerically-traced values (via the continuous solver's policy grid,
counting how many "stay" decisions occur before the first "switch") directly measure the
EXPONENT itself (matching `T^{n_e}` in the original notation), not the number of PARK steps --
the true park-step count is one less than that traced value (`park_count = traced_value - 1`),
and the return step's own exponent equals the traced value exactly (`exponent = traced_value =
park_count + 1`). Put differently: the original interpretation conflated "how many `stay`
decisions were observed in the trace" with "how many cost_a-charging park steps occur," when
actually the LAST `stay` decision in the trace (at the highest exponent before switching) itself
turns out to correspond to what should be counted as part of the transition into the return step,
not an additional independent park charge.

**Validated at 3 independent parameter points** (symmetric hop pairs, various `pi_b, lambda,
cost_a, c_switch_cold`): 2 of 3 matched to `~1e-10`-`1e-9` (machine precision) with the corrected
model at modest search ranges; the third (`pi_b=0.2, lambda=0.9, cost_a=0.15, c_switch_cold=0.05`
-- a high-persistence, relatively low-cost_a regime) needed a wider integer search range (up to
19, not 7) because its true optimal park duration is simply much longer for this parameter
combination (consistent with high `lambda`), NOT because of a remaining formula bug -- the diff
shrank monotonically as the search range widened (0.0081 -> 0.000059), confirming convergence
toward the correct value rather than hitting a floor.

**Status: the semi-Markov finite-chain construction for `g_cold(n_H, n_BB)` (symmetric hop pair,
pure Gilbert) is now validated to machine precision at multiple parameter points.** The two
"suspects" flagged by the advisor were both partially right in spirit -- Suspect A (the exponent)
was the dominant real bug; Suspect B (switch-cost count) was correctly ruled out by the isolation
experiment (2 charges per cycle, one retreat + one return, is correct, matching physical
intuition) -- but the FULL fix required recognizing that the exponent correction and the
park-count definition are coupled (not simply "add 1 to the exponent, leave everything else
unchanged"), which is exactly the kind of subtle off-by-one the advisor and Fable both warned
this specific piece of bookkeeping was prone to.

**Not yet done, but now well-scoped**: (1) reduce this validated NUMERIC finite-chain construction
to the actual symbolic sympy form (free integer symbols `n_H, n_BB`, `x_i := lambda^{n_i}` as the
advisor suggested, first differences to characterize the threshold/interior-optimum structure);
(2) the advisor separately flagged a real structural subtlety worth checking in that symbolic
pass -- for `pi_b > 1/2`, `P(both hops good)` may be NON-MONOTONE in park duration for a
one-hop-bad entry (an interior optimum, not a simple threshold), derived from
`P(both good at n) = (1-pi_b)(1-lambda^n) * [(1-pi_b) + pi_b*lambda^n]` having its vertex at
`x=(2*pi_b-1)/(2*pi_b)` which sits inside `(0,1)` exactly when `pi_b>1/2` -- this was not
numerically checked in this session and should be a priority for the next symbolic pass, since it
would mean the "always retreat immediately, park until belief crosses a fixed threshold" mental
model is incomplete in the high-loss-rate regime; (3) the asymmetric (`lambda1 != lambda2`)
generalization, and (4) combining with the warm-side closed forms into an explicit `Phi=0`
boundary equation for the symmetric pure-Gilbert slice.

## CLOSING NOTE from the advisor consultation (2026-07-19, final)

A side question arose mid-debug (before the fix above was found) about whether the residual
0.0036 discrepancy might actually be the CONTINUOUS SOLVER's own grid-interpolation error rather
than a bug in the finite-chain model -- a substantive concern, since cold-side park-phase beliefs
land on INTERIOR points of the belief simplex (unlike warm-side, where beliefs collapse to exact
grid corners every step, which is WHY the warm-side reduction matched to 1e-17 so cleanly).
**Directly tested and ruled out**: `always_cold_value_iteration`'s reported `g` is IDENTICAL to
10 decimal places across resolution in {80, 150, 300, 500} for the same benchmark parameters --
completely stable, no drift toward the finite-model's (buggy, pre-fix) value. This confirms the
0.0036 gap was entirely the finite-chain model's own `n_H`/`n_BB` miscounting bug (now fixed, see
above), not solver interpolation error. Good general lesson for this project, worth remembering:
"is X converged?" should be checked by watching the actual quantity of interest (`g`) stabilize
across resolution, not merely a downstream derived quantity (like the traced `n*`) staying put --
policy/structure stability does not by itself guarantee the VALUE has converged, though in this
specific case both turned out fine once the real bug was found.

**Clean restatement of the fix** (advisor's final phrasing, preferred over the earlier "add 1 to
the exponent" framing): the traced value from the continuous solver's policy grid (how many
consecutive "stay" decisions occur before the first "switch") directly equals the DECAY EXPONENT
for the return-step's belief (`lambda^{traced_value}`), not the number of park steps -- the
number of cost_a-charging park steps is `traced_value - 1`. This is a single, physically clean
statement (not two coupled off-by-one corrections), and should be how this is described in any
future writeup rather than the more awkward "T^{n_e+1}" framing this session arrived at first.

**Handoff for the next (symbolic) session, per the advisor's final 3 points**:
1. Use `x_i := lambda^{n_i}` (where `n_i` is the traced exponent, per the clean restatement
   above) as an independent free symbol when building `g_cold(x_H, x_BB)` as a rational function
   -- substitute `x_i = lambda^{integer}` only at the very end. This keeps the symbolic form
   readable (matches the same discipline already applied on the warm side).
2. Characterize `n*` via the sign of the first difference `g(n+1) - g(n)`, never by committing to
   a single closed-form floor/ceil expression -- this is also the right tool to automatically
   detect the interior-optimum regime (point 3 below), not just simple thresholds, and the
   concrete values found this session (e.g. `(n_H,n_BB)=(2,3)` for the first benchmark point)
   should reproduce via this first-difference check as a symbolic self-test.
3. Do not forget the `pi_b>1/2` non-monotonicity check (already flagged above) before writing any
   general characterization of `n*` -- this is a real, not-yet-verified structural risk to the
   simple "threshold" mental model, and the first-difference tool in point 2 will catch it
   automatically if present.

Session status: cold-side finite-chain construction is CONFIRMED CORRECT (validated to machine
precision, the grid-interpolation alternative explanation directly ruled out). Symbolic
(sympy) reduction of the cold side, and the two structural risk-checks above, remain for next
time -- but the numerical foundation they'd build on is now solid.

## COLD SIDE SYMBOLIC DERIVATION: DONE, 2026-07-19 (later same day), validated at 2 points

Per user request, built the full symbolic (sympy) reduction with `n_H, n_BB` as FREE symbols
(not committed to specific integers) and `x_H := lambda**n_H`, `x_BB := lambda**n_BB` as
independent symbols, following the advisor's plan and this session's "clean restatement"
convention (`n_i` = the traced decay exponent directly; park-step count = `n_i - 1`). See
`pure_gilbert_symbolic_cold_demo.py`.

**Construction**: closed-form single-hop n-step transition (`P(Bad at exponent n | started
Good) = pi_b*(1-x)`, `P(Bad at exponent n | started Bad) = pi_b + x*(1-pi_b)`, no matrix powers
needed), joint via independence (rho=0), Markov-renewal-reward over the embedded chain {H, BB}
using the absorbing/fundamental-matrix construction for the B-ride phase (GG as sole transient
state, `N=1/(1-q_G^2)`, using the SAME `f_GG` exit distribution already validated on the warm
side as a spot check). Embedded transition matrix's left eigenvector `nu` solved exactly via
`sp.solve`; `g_cold = (nu_H*R_H + nu_BB*R_BB)/(nu_H*tau_H + nu_BB*tau_BB)`.

**A real substitution-error scare, resolved (worth recording as its own lesson)**: the first
numeric check used `n_H=2, n_BB=3` and got `diff=0.0051` -- looked like a NEW formula bug, but
turned out to be a **stale-convention substitution error**, not a formula error: `(2,3)` were
values from the EARLIER debugging session's now-abandoned "`T^{n_e+1}`" convention (where the
free variable meant "park count", requiring `+1` for the exponent), not this module's "clean
restatement" convention (where the same-named variable means the exponent directly). Re-running
with the ORIGINAL traced values `n_H=3, n_BB=4` (which are exactly what "clean restatement"
calls for) gave `diff=2.09e-10` -- confirming the SYMBOLIC FORMULA was correct all along; only
the substitution was wrong. **Lesson**: when a session juggles two successive namings for the
same underlying quantity (here: "park count" vs "exponent," differing by exactly 1), always
re-derive or explicitly restate which convention a given numeric constant belongs to before
substituting it into a later, differently-conventioned formula -- don't carry a bare number
across a renaming without re-deriving it.

**Validated at 2 independent parameter points** (both machine precision):
- `pi_b=0.1, lambda=0.5, cost_a=0.30, c_switch_cold=0.10, n_H=3, n_BB=4`: symbolic
  `g=0.174069319640565` vs continuous solver `g=0.174069319431571`, diff=`2.09e-10`.
- `pi_b=0.05, lambda=0.6, cost_a=0.5, c_switch_cold=0.03, n_H=2, n_BB=3`: symbolic
  `g=0.0930772244316035` vs continuous solver `g=0.09307722516278472`, diff=`-7.31e-10`.

**A genuine structural observation from the algebra itself** (not yet fully interpreted):
`R(H) = R(BB) = cost_a*(n_i - 1) + 1 + 2*c_switch_cold` -- the per-cycle expected cost, aside
from the linear park-cost term, reduces to EXACTLY the same constant (`1 + 2*c_switch_cold`)
regardless of entry type or of `pi_b`/`lambda`. This falls directly out of the identity
`(1 - P_GG) + P_GG*1 = 1` (the return step's own expected loss plus the conditional expected
GG-ride cost, using the `N*(1-q_G^2)=1` renewal identity, always sums to exactly 1) -- a clean,
parameter-independent cancellation, not an approximation. All of the entry-type/persistence-
dependent structure in `g_cold` therefore lives entirely in `tau_H` vs `tau_BB` (the expected
CYCLE LENGTHS differ) and in the embedded chain's stationary weights `nu_H, nu_BB` -- NOT in the
per-cycle reward `R`. This is a genuine, validated simplification worth stating explicitly in
any future writeup, and is exactly the kind of "readable core object" the advisor's scoping
guidance (item C, "don't aim for a monster, aim for readable structure") was steering toward.

**Status: cold-side symbolic derivation is DONE, matching the warm side's status.** Both halves
of the pure-Gilbert symmetric-hop warm/cold boundary now have validated, machine-precision-
checked closed/semi-closed forms:
- `g_warm`: fully explicit closed forms for 3 named policies (Section "Part A" above).
- `g_cold(n_H, n_BB)`: an explicit rational function of `(pi_b, lambda, cost_a, c_switch_cold,
  n_H, n_BB)` via `x_H=lambda^{n_H}, x_BB=lambda^{n_BB}`, with the clean `R_H=R_BB` simplification
  above; `n_H, n_BB` themselves still need the first-difference optimization (not yet done
  symbolically, though numerically straightforward) to become the TRUE optimal `n*`.

**Remaining for a future session -- FINAL PRIORITY ORDER per the advisor's closing message**
(do NOT reorder this -- doing (2)/(3) before (1) risks silently assuming "n* is a simple
threshold" and getting the cold-side optimum wrong before ever reaching Phi=0):

1. **`pi_b>1/2` non-monotonicity check, FIRST, before any `n*` characterization work.** For a
   one-hop-bad entry, the recovering hop's `beta_bad(n)` (decaying toward `pi_b` from 1) competes
   against the still-good hop's `beta_good(n)=pi_b*(1-lambda^n)` (degrading toward `pi_b` from 0),
   and `P(GG|return)` can be non-monotone (interior-optimum) in `n` when `pi_b>1/2`. The
   first-difference tool in point 2 below automatically catches an interior optimum if one
   exists (no separate tool needed) -- but this must be CHECKED (e.g. `pi_b in {0.6, 0.8}`,
   one numeric point each against the continuous solver) before trusting any general
   characterization of `n*`, since a silent threshold-only assumption could misidentify the
   cold-side optimal policy going into step 3.
2. Characterize `n*` via `g_cold(n+1)-g_cold(n)`'s sign (the ingredients -- `tau`, `R`, `nu` --
   are all in hand from this session). Domain `n>=2`; treat `n=1`-equivalent ("never retreat,
   ride through") and `n->infinity` ("permanent park") as separate degenerate branches to compare
   against, mirroring how warm-side's always-A/always-B are handled as their own named policies
   rather than limits of a general formula.
3. Combine warm + cold into an explicit `Phi=0` boundary equation for the symmetric pure-Gilbert
   slice -- LAST, only after each side's own optimal policy/`n*` is pinned down (both sides
   effectively require a "min over policies" resolved first, so `Phi` is a difference of two
   already-optimized quantities, not two general formulas).
4. The asymmetric (`lambda1 != lambda2`) generalization remains explicitly out of scope until the
   symmetric case is fully clean end-to-end (unchanged from the original scoping decision).

## pi_b>1/2 NON-MONOTONICITY: CONFIRMED REAL, 2026-07-19 (continued) -- first-difference-only search is INSUFFICIENT

Per the agreed priority order (check this BEFORE trusting any `n*` characterization), verified
the advisor's predicted non-monotonicity both at the belief level and at the `g_cold` level.

**Belief-level (closed form, no solver involved)**: `P(GG|return)` for a one-hop-bad entry, as a
function of `x=lambda^n`, is monotone for `pi_b<=0.5` and has a genuine INTERIOR MAXIMUM for
`pi_b>0.5`, with vertex location matching the advisor's predicted formula
`x=(2*pi_b-1)/(2*pi_b)` EXACTLY (e.g. `pi_b=0.6` -> predicted `0.1667`, measured `0.1666`;
`pi_b=0.8` -> predicted `0.375`, measured `0.3752`). This is a clean, closed-form-confirmed fact,
not a numerical artifact -- computed directly from the algebraic formula, no grid/solver
involved at all.

**Solver-level sanity check (initially confusing, resolved)**: tracing the continuous solver's
actual policy for `pi_b=0.6, lambda=0.9, cost_a=0.5` at resolution 60/100 showed apparent
"switch-then-stay-then-switch" flickering, which INITIALLY looked like direct evidence of a
non-monotone POLICY -- but this flickering disappeared at resolution=200 (clean single
threshold). This was almost certainly grid-interpolation noise near the decision boundary, NOT
the real phenomenon (this parameter point's `cost_a=0.5` also turned out to be poorly calibrated,
far below the ~0.84 stationary path-B loss for `pi_b=0.6`, pushing the system toward the
"never return" degenerate regime where this specific check isn't very informative). **Lesson**:
when hunting for a subtle non-monotonicity, do not trust a single mid-resolution solver trace --
either use the validated closed-form `g_cold` symbolic expression directly (no grid at all), or
push solver resolution much higher before concluding a flicker is real.

**`g_cold`-level (the quantity that actually matters for finding the true optimum) -- CONFIRMED
GENUINELY MULTI-MODAL, not just a single interior max/min**: using the validated symbolic
`g_cold_expr` (via `sp.lambdify` for speed -- plain `sp.Rational` substitution was too slow for a
broad sweep), found a concrete, real example at `pi_b=0.7, lambda=0.7, cost_a=0.9,
c_switch_cold=0.1, n_BB=30`: sweeping `g_cold(n_H)` for `n_H=2..59` gives a value that
DECREASES from `n_H=2` to a LOCAL MINIMUM around `n_H=4` (`g~0.90388`), then INCREASES to a LOCAL
MAXIMUM around `n_H=8-9` (`g~0.90403`), then DECREASES AGAIN monotonically all the way to
`n_H=59` (`g~0.90189` -- LOWER than the earlier "local minimum"). **This means the local minimum
at n_H~4 is NOT the true optimum** -- a naive "stop at the first sign change in the first
difference" search would incorrectly report `n*~4` when the real optimum is further out (possibly
requiring the `n_H->infinity` limit to resolve where the true global minimum actually sits).

**CORRECTION to the earlier handoff note**: "characterize `n*` via the sign of
`g_cold(n+1)-g_cold(n)`" is necessary but NOT SUFFICIENT on its own when `pi_b>1/2` -- the search
must consider the GLOBAL trend (e.g. evaluate a wide range of `n`, or take the `n->infinity`
limit explicitly and compare against all local minima found), not stop at the first local
optimum. This is exactly the kind of subtlety the advisor's original warning was about, now
concretely confirmed rather than just theoretically anticipated.

## DECISIVE CONFIRMATION: a real interior-GLOBAL-optimum window exists, located via a single-entry toy model

Per the advisor's suggested simplification (isolate the phenomenon in a single-entry, non-embedded
toy model to avoid embedded-chain complexity clouding the question): `g(n) = (A + B*n) /
(n + C*P_GG(n))` where `A=1+2*c_switch_cold-cost_a`, `B=cost_a`, `C=1/(1-q_G^2)`,
`P_GG(n)=(1-pi_b)*(1-x)*((1-pi_b)+pi_b*x)`, `x=lambda^n` (one-hop-bad entry). This isolates
exactly the mechanism in question (the `P_GG` bump feeding only into the denominator/timing, per
the already-validated `R=1+2c_switch_cold` cost-side invariant) without the 2-state embedded-chain
machinery.

**Swept `cost_a` systematically for `pi_b=0.8, lambda=0.85, c_switch_cold=0.05`** (stationary
path-B loss for this pair `=pi_b*(2-pi_b)=0.96`): for `cost_a` in `[0.50, 0.84]`, the global
minimum of `g(n)` over `n=2..199` sits at the search boundary (`n=199`, i.e. the plateau/`n*=
infinity` regime dominates). But **starting at `cost_a=0.86` and continuing through `0.96`, the
global minimum genuinely relocates to a FINITE INTERIOR value**: `n*=6` at `cost_a=0.86`, `n*=5`
at `0.88-0.90`, `n*=4` at `0.92-0.94`, `n*=3` at `0.96` -- shrinking smoothly as `cost_a`
approaches the stationary loss. **This is a clean, confirmed, real phenomenon**: for
`pi_b>1/2`, there is a genuine window of `cost_a` (here, roughly the top ~10% of the range below
the stationary path-B loss) where the optimal park duration is a true interior value driven by
the `P(GG|return)` bump, not simply "threshold" (small finite `n*` from a monotone `P_GG`) or
"plateau" (`n*=infinity`). Below this window, the plateau dominates; a genuine threshold-only
regime (interior optimum absent, `n*` small and driven by monotone decay, as in the `pi_b<=1/2`
case) was not separately re-confirmed in THIS specific sweep but is expected to reappear for
`pi_b<=1/2` per the belief-level analysis above.

**Honest calibration of the earlier "noise vs real" ambiguity**: the `|Delta g|` magnitude check
(requested by the advisor to distinguish real structure from rounding noise) found values of order
`1e-3` to `1e-6` in the earlier 2-embedded-state numeric explorations -- clearly ABOVE the
`~1e-12` pure-rounding-noise floor the advisor hypothesized, but this ambiguity is now moot: the
single-entry toy model's clean `cost_a`-sweep result is decisive on its own (a smoothly-shifting
finite `n*` across a clear `cost_a` window is unambiguously real structure, not noise), so the
earlier embedded-2-state numeric findings (which showed a similar but messier pattern, likely
because `n_H` and `n_BB` interact through the shared embedded-chain stationary weights `nu`) can
now be understood as the SAME underlying phenomenon, just harder to read cleanly in the full
2-state system directly.

**Status: `pi_b>1/2` non-monotonicity is CONFIRMED REAL at both the belief level (closed form)
and the `g`/optimal-`n*` level (via the single-entry toy model's clean `cost_a`-sweep) -- this is
not a numerical artifact and not merely a theoretical possibility.** The simple "threshold or
plateau" mental model from the `pi_b<=1/2` case does NOT cover the full picture for `pi_b>1/2`;
a genuine third regime (interior optimal park duration) exists in a real, identifiable window of
`cost_a` near the stationary path-B loss.

## CONFIRMED IN THE FULL 2-STATE EMBEDDED CHAIN TOO (`pure_gilbert_cold_full_embedded_sweep_demo.py`)

Ran the SAME `cost_a` sweep directly on the exact, validated `g_cold_expr(n_H, n_BB)` (both H and
BB entries jointly, via the real embedded-chain stationary weights `nu` -- no single-entry
simplification), doing a full 2D grid search over `(n_H, n_BB) in [2,200]^2` at each `cost_a`, for
`pi_b=0.8, lambda=0.85, c_switch_cold=0.05` (same point as the toy sweep):

```
cost_a   argmin(n_H,n_BB)   g_min       at N_MAX=200 boundary?
0.50     (200, 200)         0.502554    True
0.70     (200, 200)         0.701378    True
0.80     (200, 200)         0.800790    True
0.84     (200, 200)         0.840555    True
0.86     (6,   200)         0.860433    True   <- n_H goes interior FIRST
0.88     (6,   200)         0.880218    True
0.90     (5,   50)          0.899983    False  <- n_BB now interior too
0.92     (5,   23)          0.918620    False
0.94     (4,   18)          0.936727    False
0.96     (4,   15)          0.954435    False
```

**This is a stronger, richer confirmation than the toy model alone showed**: the transition is
not a single simultaneous jump but happens in TWO STAGES -- `n_H*` (the one-hop-bad entry, which
is the one that actually has the `pi_b>1/2` interior-maximum belief bump) relocates to a finite
interior value FIRST (at `cost_a~0.86`, matching the toy model's threshold almost exactly), while
`n_BB*` (which has no such bump -- both hops already bad, `P(GG|return)` is monotone in `x_BB`)
stays pinned at the search boundary until `cost_a~0.90`, then also comes down. This makes sense
structurally: `n_BB*`'s own optimal value is driven purely by the ordinary threshold trade-off
(park longer = save `cost_a` now, at the price of a worse expected return-step loss later, no
bump involved), so it only starts shrinking once `cost_a` gets close enough to the stationary loss
for the ordinary threshold mechanism to bite -- `n_H*`'s bump-driven relocation is the genuinely
new, `pi_b>1/2`-specific effect, and it kicks in at a lower `cost_a` than the ordinary
threshold effect on `n_BB*` does.

**Conclusion: the single-entry toy model's finding transfers correctly to the full system -- the
window is real, not a toy-model artifact, and the full system reveals it decomposes into two
distinct sub-effects (bump-driven `n_H*` transition first, ordinary-threshold-driven `n_BB*`
transition second).** This closes off the last open question from the previous handoff
("does this survive in the full embedded chain, or was the toy model insufficient") -- verified
directly rather than left to the advisor's judgement call.

**Next-session priority, updated**: (a) map the `n_H*` bump-transition threshold symbolically
(where exactly does `n_H*`'s global minimum leave the `n=infinity` plateau, as a function of
`pi_b, lambda, cost_a` -- likely tractable since it's driven by the same `P(GG|return)` vertex
formula already in closed form), (b) then map `n_BB*`'s ordinary threshold separately (simpler,
standard renewal-reward threshold argument, no bump), (c) only then proceed to the general `n*`
search strategy: evaluate a sufficiently wide range and take the true minimum (do NOT stop at the
first first-difference sign change) as the safe default, given a threshold-only assumption is now
confirmed unsafe for `pi_b>1/2`.

## CLOSED-FORM `n_H*` BUMP-TRANSITION THRESHOLD -- DERIVED AND EXACTLY VERIFIED

Item (a) above turned out to have an exact, clean closed form. Key algebraic trick in the
single-entry toy `g(n) = (A+B*n)/(n+C*P_GG(n))` (`A=1+2*c_switch_cold-cost_a, B=cost_a, C=N`):
since the plateau value `g(n->infinity) = B = cost_a` exactly (the `n` terms in numerator and
denominator both scale linearly with slope `B` vs `1`, so `g(n)->B` as `n->infinity`), the sign
of `g(n) - cost_a` collapses beautifully:

```
g(n) - cost_a = [A + cost_a*n - cost_a*n - cost_a*C*P_GG(n)] / (n + C*P_GG(n))
              = [A - cost_a*C*P_GG(n)] / (n + C*P_GG(n))
```

The `n`-linear terms cancel EXACTLY (because `B=cost_a` is precisely the plateau's own cost
coefficient) -- so whether ANY finite `n` beats the plateau reduces to a single inequality with no
`n`-dependence in the denominator's sign (`n+C*P_GG(n)>0` always): **`g(n) < cost_a` iff
`P_GG(n) > A/(cost_a*C)`**, i.e. iff `P_GG(n) > (1+2*c_switch_cold-cost_a)/(cost_a*N)`.

Since `P_GG(n)`'s continuous-`x` maximum (at the already-known vertex `x*=(2*pi_b-1)/(2*pi_b)`,
valid for `pi_b>1/2`) has the clean closed form `P_GG_max = (1-pi_b)/(4*pi_b)` (derived by
substituting `x*` back into `P_GG = (1-pi_b)*(1-x)*(1-pi_b+pi_b*x)` -- the `(1-x*)` and
`(1-pi_b+pi_b*x*)` factors both simplify to `1/(2*pi_b)` and `1/2` respectively), **an interior
optimum can beat the plateau AT ALL iff**:

```
(1-pi_b)/(4*pi_b)  >  (1+2*c_switch_cold-cost_a) / (cost_a * N)
```

Solving the equality for the exact crossing point gives a clean closed-form threshold:

```
cost_a* = (1 + 2*c_switch_cold) / (1 + N*(1-pi_b)/(4*pi_b))
```

where `N = 1/(1-q_G^2)`, `q_G = 1 - pi_b*(1-lambda)` (the single-hop `P(Good|Good)` persistence).

For `cost_a > cost_a*`: a genuine interior `n_H*` exists that beats "park forever." For
`cost_a <= cost_a*`: the plateau (park forever / `n_H*=infinity`) is globally optimal, no interior
point can win, REGARDLESS of the bump's existence -- the bump alone is not sufficient, its height
must clear this specific cost-ratio bar.

**Verified EXACTLY** (not just approximately) at `pi_b=0.8, lambda=0.85, c_switch_cold=0.05`:
formula gives `cost_a*=0.8613675807...`; a fine-resolution sweep of the toy model right around
this value shows the sign of `g(n)-cost_a` (at the argmin, `n=6`) flips from `False` at
`cost_a=0.86087` to `True` at `cost_a=0.86187` -- bracketing the closed-form value to 3+ decimal
places, i.e. an exact match (see `/tmp/.../scratchpad/verify_threshold.py` and
`verify_threshold2.py` for the derivation/sympy work and the bracketing check). This also matches
the previously-observed discrete transition in both the toy sweep (`0.84->0.86`) and the full
embedded-chain sweep (`n_H*` leaves the boundary between `cost_a=0.84` and `0.86`) -- all three
independent views (closed-form algebra, toy numeric sweep, full embedded-chain numeric sweep)
agree to the precision each is capable of.

**Caveat**: this closed form solves the CONTINUOUS-`x` relaxation (as if `n` could be any real
number, so `x=lambda^n` could hit the vertex exactly). The true discrete optimum only needs
`x*` to be well-approximated by SOME integer `n>=2` (`n*=log(x*)/log(lambda)`); for this parameter
point `n* = ln(0.375)/ln(0.85) ≈ 6.04`, matching the observed discrete `n_H*=6` almost exactly.
For parameter regions where the continuous `n*` rounds far from an achievable integer, or is
`<2` (domain violation), this threshold formula would need a small correction (compare `g` at the
two bracketing integers explicitly rather than trusting the continuous relaxation) -- not yet
checked, flagged as a follow-up caveat rather than a confirmed gap.

## `n_BB*` THRESHOLD -- SAME TRICK APPLIES, ALSO CLOSED FORM (item (b) done)

The `A - cost_a*C*P_GG(n)` sign-collapse trick used for `n_H*` is entirely general (it only used
`B=cost_a`, which holds for BOTH entry types -- `R(H)=R(BB)` was already established as an exact,
entry-independent structural identity, see above). For BB entry, `P_GG_BB(n) =
(1-pi_b)^2*(1-x)^2` (`x=lambda^n`) is monotonically INCREASING in `n` (no bump -- consistent with
belief-level expectation, since both hops start Bad so there's no "one hop still fresh" effect to
peak against), saturating at `f_max = (1-pi_b)^2` as `n->infinity`. Since `P_GG_BB` is monotone
(not bump-shaped) and NEVER exceeds `f_max`, the exact same threshold construction gives:

```
cost_a*_BB = (1 + 2*c_switch_cold) / (1 + N*(1-pi_b)^2)
```

**Verified exactly** in the standalone BB-only toy at the same parameter point (`pi_b=0.8,
lambda=0.85, c_switch_cold=0.05`): closed form gives `cost_a*_BB=0.9343373...`; fine sweep (with
`N_MAX=2000` to be safely away from the search boundary) shows the sign of "beats plateau" flip
exactly between `cost_a=0.93384` (False, argmin sits at the 2000 boundary) and `cost_a=0.93484`
(True, argmin=53) -- bracketing the closed form to 3+ decimal places, same quality of match as the
H-side.

**Interesting subtlety (a genuine, non-obvious finding, not just a formality)**: even though
`P_GG_BB` has NO bump, `g_BB(n)` still has a genuine INTERIOR minimum once `cost_a>cost_a*_BB` --
not just "improves forever as `n->infinity`". This is because `g_BB(n)` approaches the plateau
value `cost_a` from BELOW as `n->infinity` (since `P_GG_BB` saturates at a FINITE value rather than
diverging, the negative numerator `A-cost_a*C*P_GG_BB(n)` converges to a finite negative constant,
so `g_BB(n)-cost_a ~ const/n -> 0^-`), meaning `g_BB` dips to some finite minimum and then rises
back UP toward (but never reaching) the plateau value as `n` grows further -- so "monotone bump,
no interior optimum" was the WRONG mental model even for the "ordinary" BB entry; every entry type
in this system has a genuine interior optimum once past its own threshold, the only difference
between H and BB is the shape of `P_GG(n)` (bump vs monotone-saturating), not whether an interior
optimum exists at all.

**Coupling caveat (important, explains an apparent discrepancy)**: the standalone BB-toy threshold
(`cost_a*_BB=0.9343`) is noticeably HIGHER than where `n_BB*` was observed to leave the boundary
in the FULL embedded-chain sweep above (between `cost_a=0.88` and `0.90`, i.e. ~0.03-0.05 lower
than the isolated-toy prediction). This is because the two entry types are COUPLED through the
shared embedded-chain stationary weights `nu` (`g_cold` is a `nu`-weighted average of the two
per-cycle rates, and `nu` itself depends on both `n_H` and `n_BB` via the `P_to_H`/`P_to_BB`
transition probabilities) -- once `n_H` becomes finite (at `cost_a>0.86`), the mix of cycle types
shifts, which measurably shifts the EFFECTIVE threshold at which `n_BB*` also becomes interior,
relative to what the fully-isolated single-entry toy predicts. **The single-entry toy thresholds
are accurate for each entry type analyzed alone, but the coupled system's actual joint transition
points differ slightly (order 0.03-0.05 in `cost_a` here) once one side has already gone
interior** -- worth flagging explicitly rather than treating the isolated-toy thresholds as exact
joint-system predictions.

**Status of the priority list**: (a) `n_H*` closed-form threshold: DONE. (b) `n_BB*` closed-form
threshold: DONE (same trick, both entries share the `R=1+2c_switch_cold`-derived plateau-value
cancellation). (c) general `n*` search strategy: given BOTH entry types have a genuine interior
optimum once past their own (coupling-shifted) threshold, and NEITHER can be found by a
first-difference sign check alone in general (though in practice, since `P_GG_BB(n)` is monotone,
`g_BB(n)` alone -- ignoring coupling -- is unimodal past threshold, so ordinary first-difference
search IS safe for a BB-only analysis; it is specifically the `pi_b>1/2` H-side bump that breaks
first-difference safety), the safe default remains: evaluate a sufficiently wide range and take
the true minimum. Remaining open item: combine into an explicit `Phi=0` warm/cold boundary
equation (priority (3)), now that both cold-side thresholds are in closed form.

## ADVISOR CORRECTIONS AND FOLLOW-UP CHECKS (2026-07-19, later round)

The advisor caught up on the reports above and made three points, addressed here:

**(1) Corrected earlier advice**: the advisor's OWN earlier suggestion ("characterize `n*` via
first-difference sign") was wrong for `pi_b>1/2`, confirmed by the advisor itself, not just by my
own numeric check. Exact correct algorithm given: enumerate all local minima of `g(n)` for
`n` in `[2, n_settle]` (`n_settle` ~ a few multiples of `1/(1-lambda)`, where `lambda^n` has
converged), add the plateau candidate `g(infinity)=cost_a`, and take the global min of that finite
set. **Decision rule: `n*` is finite iff some local min `< cost_a`; otherwise `n*=infinity`**
(degenerate plateau).

**(2) Location vs. threshold -- an important distinction, now resolved**: the advisor pointed out
I had conflated two different quantities: (A) the LOCATION of the interior local min,
`n_H*(cost_a)`, and (B) the DETACHMENT THRESHOLD `cost_a*(pi_b,lambda)` at which that local min
first beats the plateau. My derivation of (B) via the vertex substitution is EXACT, not merely a
leading-order approximation, for a reason the advisor's general resultant-based method doesn't
need but my special-case one exploits: since `g(n)-cost_a` and `A-cost_a*C*P_GG(n)` share the
SAME SIGN for every `n` (proven exactly via the `B=cost_a` cancellation), and the right-hand
quantity is a strictly monotonic (decreasing) function of `P_GG(n)` alone with an `n`-INDEPENDENT
comparison threshold `A/(cost_a*C)`, the question "does any n make `g(n)<cost_a`" reduces exactly
to "does `max_n P_GG(n)` exceed the threshold" -- and `max_n P_GG(n)` occurs (up to integer
rounding) exactly at `x*`. So `cost_a*` computed via `x*` is exact for the DETACHMENT question.
**However, (A) the location `n_H*(cost_a)` for `cost_a` strictly ABOVE `cost_a*` is NOT
generally at `x*`** -- confirmed by the data itself: `x*` (hence its associated integer
`n≈6.04`) is FIXED (depends only on `pi_b,lambda`), but the observed `n_H*` shifts with `cost_a`
(6 at 0.86, 5 at 0.90, 4 at 0.94, 3 at 0.96) -- so tracking the interior location as a function of
`cost_a` for `cost_a>cost_a*` is a genuinely separate (and not yet closed-form) problem from the
detachment threshold, and the advisor's caution here was correct and worth recording explicitly.
**Scope decision: since the `Phi=0` boundary work (priority 3) only needs to know WHETHER an
interior cold-side optimum exists and roughly what value it achieves relative to the warm side,
not the exact `n*(cost_a)` location curve, the closed-form detachment thresholds (already done for
both `n_H` and `n_BB`) are sufficient for that purpose -- the location-tracking problem is
deferred as an unnecessary refinement for now.**

**(3) Full-model 2D joint check requested by the advisor -- ALREADY SATISFIED by prior work**:
the advisor asked for one full-embedded-chain check at the window point `cost_a=0.90` with a
TRUE JOINT 2D `(n_H,n_BB)` grid search (not one variable fixed), to confirm the interior joint
optimum survives the `nu`-coupling. This is exactly what
`pure_gilbert_cold_full_embedded_sweep_demo.py` already computed (the script does a genuine 2D
`np.meshgrid` joint search, not a fixed-one-variable slice): at `cost_a=0.90`, joint argmin =
`(n_H,n_BB)=(5,50)`, `g_min=0.899983 < cost_a=0.90`, confirmed NOT at either search boundary. This
independently satisfies the advisor's requested check -- no further re-run needed.

**(4) Real-data relevance check (requested by advisor, done here)**: re-ran the Berlin V2X
block-fit (`berlin_v2x_block_fit_demo.py`) to get the actual calibrated EM parameters: hop1
`p_gb=0.1909, p_bg=0.4553` (`pi_b=0.2954`), hop2 `p_gb=0.2764, p_bg=0.3933` (`pi_b=0.4127`).
**BOTH real calibrated hops have `pi_b<1/2`** -- i.e., the `pi_b>1/2` non-monotonicity/interior-
optimum-window phenomenon does NOT apply to either hop in this project's only credible real-data
calibration. Additionally, both real hops have `eps_good`/`eps_bad` far from the pure-Gilbert
idealization (`eps_good~0.03-0.07, eps_bad~0.30-0.43`, not `0`/`1`) -- the exact pure-Gilbert
reduction this whole thread analyzes doesn't even directly apply to these real channels (a
different, already-known limitation, see the `eps_bad=1` retraction earlier in
`TRACE_CALIBRATION_NOTES.md`). **Conclusion: the `pi_b>1/2` interior-optimum phenomenon is a
confirmed, real, closed-form-characterized structural feature of the pure-Gilbert idealization,
but is currently a THEORETICAL CORNER CASE with no empirical footprint in this project's
calibrated real data** -- worth stating plainly in any writeup so it isn't over-sold as
practically significant, while still being a genuine, non-obvious, fully-resolved mathematical
finding in its own right.

## CORRECTION (same-session, later): the "two-stage" full-embedded-chain finding was itself a resolution artifact

The advisor pushed back hard (correctly) on the `n_BB*` closed-form threshold above: unlike
`n_H`'s bump (which has a genuine finite vertex where `P_GG'=0`, making the "value=plateau AND
slope=0" conditions coincide at one finite, algebraically clean point), `P_GG_BB(n)` is strictly
monotone with NO finite vertex -- so `n_BB*`'s detachment from the plateau is a **transcendental
"fold at infinity"**, not a clean algebraic tangency: as `cost_a` approaches `cost_a*_BB` from
above, the location `n_BB*` that first beats the plateau grows without bound (verified directly:
argmin `n` at `cost_a*_BB+delta` for `delta=0.05,...,0.00005` grows `20,27,32,...,69` -- receding
continuously, never a sharp jump to a fixed finite value the way `n_H*` jumps to `~6`).

**This forced a re-examination of the "two-stage" full-embedded-chain claim** (`n_H*` interior at
`cost_a=0.86`, `n_BB*` following at `cost_a~0.90`), which turned out to itself be a resolution
artifact of the earlier `N_MAX=200` (then `2000`) grid search -- NOT because `n_H*=6` was wrong
(it wasn't -- confirmed robust below), but because **the search never pushed `n_BB` far enough to
see that at `cost_a=0.86-0.895`, the TRUE optimal `n_BB` continues to recede past 200, past 2000,
past 200,000, all the way to at least 5,000,000** (fold-at-infinity, exactly as the advisor's
structural argument predicted) -- meaning the joint system had NOT actually detached from the
`(n_H,n_BB)=(infinity,infinity)` plateau at `cost_a=0.86` at all, contrary to the earlier report.

**Properly resolved picture** (verified with `n_BB` search range up to 5,000,000, log-spaced):
- `n_H*=6` genuinely IS the best available value for the H-component alone, robustly confirmed
  even when `n_BB` is pushed to 5,000,000 or when `n_H` is compared against much larger
  alternatives (e.g. `g(n_H=6,n_BB=200000)=0.86000044 < g(n_H=60,n_BB=200000)=0.86000068`) -- so
  the CLOSED-FORM `n_H*` bump-threshold (`cost_a*_H=0.8614`) remains exactly correct as a
  statement about the H-cycle's own optimal duration.
- BUT the JOINT system's average cost `g_cold` does NOT actually beat the plateau value `cost_a`
  at all until `n_BB` ALSO crosses ITS OWN threshold -- because with `n_BB` still receding toward
  infinity, the embedded chain's stationary weight `nu_BB -> 1` (BB-cycles, being far longer,
  dominate the time-average), so `g_cold` is governed almost entirely by the BB-cycle rate and
  converges to `cost_a` regardless of what `n_H` is doing. Bisecting precisely: the joint
  "beats-plateau" flip happens between `cost_a=0.8995` (still false, `n_BB` pinned to the
  5,000,000 search tail) and `cost_a=0.8997` (true, `n_BB*=58` genuinely finite) -- i.e. the
  **coupled joint-system threshold is `~0.8996`**, clearly BELOW the standalone BB-only threshold
  `cost_a*_BB=0.9343` (confirming the coupling-driven downward shift I originally speculated,
  more precisely now: shift magnitude `~0.0347`, not the earlier rough `0.03-0.05` guess from the
  under-resolved `N_MAX=200/2000` runs).
- **Restated correctly**: `n_H`'s bump-driven threshold (`0.8614`) governs where `n_H*` SITS once
  the overall system has detached, but does NOT by itself cause the joint system to detach from
  the plateau -- detachment of the WHOLE system is gated by whichever component's threshold is
  reached LAST when accounting for the coupling (here, `n_BB`'s coupled threshold `~0.8996`, since
  `n_BB`'s cycles dominate the time-average whenever `n_BB` is still receding). The earlier
  "two-stage, `n_H` first at `0.86`" language wrongly implied partial, real detachment already at
  `0.86` -- that was not so; the system stays in the true, undetached plateau throughout
  `[cost_a*_H, ~0.8996)`, only trivially preferring `n_H=6` within that dead range because it's a
  free, already-optimal-for-later choice that costs nothing extra while `n_BB` is still receding.

**Net effect on prior conclusions**: this does NOT change the earlier real-data-relevance
conclusion (Berlin V2X `pi_b` both `<1/2`, so this entire `pi_b>1/2` machinery remains a
theoretical corner case) -- but it DOES correct the internal narrative of how the joint system
detaches, and is recorded as a caution: **grid searches over the cold-side finite-MDP reduction
must push the receding-side variable's range far past what "looks converged" at a given
resolution before trusting a detachment claim** -- this is the third time in this thread
resolution artifacts have produced a plausible-looking but wrong intermediate conclusion (grid
policy-trace non-monotonicity, `N_MAX=80..500` solver-`g` stability check, and now this), so it's
worth stating as a standing methodological lesson for this whole research thread, not just this
specific finding.

**Advisor's precision refinement on the gate/shift language** (both correct, not contradictory):
the joint system's detachment is GATED by whichever entry is slower to detach (here, `n_BB`, since
while it's still receding it's the absorbing/dominant-`nu` entry) -- but `n_H`'s own early
detachment is not "irrelevant," it actively SHIFTS the gate's threshold downward (mixing in
`n_H`'s already-sub-`cost_a` cycles lets the joint average dip below `cost_a` at a lower `cost_a`
than the standalone-BB value would allow). **Precise statement: system detachment is GATED by the
slower entry (`n_BB` here), and that gate's threshold is SHIFTED by the faster entry's (`n_H`'s)
prior detachment.** Both descriptions (mine and the advisor's) agree; this phrasing is kept as the
canonical one going forward.

**STANDING METHODOLOGICAL GUARD (explicit, since this is the third occurrence in this thread)**:
1. **An argmin landing at or near a search range's edge means "not yet converged," never "looks
   converged"** -- extend the range until the optimum sits strictly in the interior before trusting
   any detachment/threshold claim built on it.
2. **Judge "plateau vs. finite-park" by comparing the finite-grid minimum against the ANALYTIC
   limit `g(infinity)=cost_a`, not against an empirical value read off the grid's own boundary** --
   this makes the plateau test invariant to how far the grid was actually extended (whether `n_BB`
   maxes out at 200 or 5,000,000 no longer matters for the yes/no judgment, only for finding the
   actual interior optimum once one is known to exist).
3. This specific cold-side reduction is unusually prone to (1) because the receding variable
   decays only geometrically (`lambda^n`), so it can take many orders of magnitude of grid range to
   visibly separate "still receding" from "genuinely converged interior."
This is now the third time a resolution/grid-range artifact produced a plausible-but-wrong
intermediate conclusion in this thread (solver policy-trace non-monotonicity at low resolution;
the `N_MAX=80..500` value-stability check that WAS properly done and did rule out one such
artifact; and now this one) -- treat it as a standing checklist item for any future numeric work
on this cold-side reduction, not a one-off fix.

**This closes out the `pi_b>1/2` non-monotonicity investigation, now with the corrected joint
picture.** Next: proceed to priority (3), the `Phi=0` warm/cold boundary equation, using the
simpler (`pi_b<=1/2`-style, monotone, no-bump) mental model as the DEFAULT case (matching real
data), with the `pi_b>1/2` interior window (now correctly characterized, including the coupling
subtlety) noted as a documented exception requiring the closed-form thresholds and the coupled-
threshold caveat above if ever `pi_b>1/2` real data is encountered.

## MAJOR REFRAME (2026-07-19, later round): boundary LOCATION is ill-conditioned; distill the GAP BOUND instead

Following a user suggestion ("a distilled approximate formula with a small, honestly-quantified
error would be more engineering-interesting than an exact result confined to an idealized special
case"), attempted to test whether the pure-Gilbert `cost_a*` closed form (§ above) could serve as
a cheap, no-solver-needed APPROXIMATION for the general (real, partial-observation) case, by
plugging real `eps_good`/`eps_bad` into the eps-independent formula and measuring the error
against the true crossing (found via the general `switching_curves` solver).

**Result: this failed, not gracefully but QUALITATIVELY** -- moving `eps_good`/`eps_bad` even a
small fraction of the way from the pure-Gilbert idealization `(0,1)` toward a real hop's own
calibrated values caused the predicted crossing to **vanish entirely** (no sign change found in a
wide `cost_a` search), rather than drifting with a gracefully growing error. A quick "effective
symmetric hop" attempt to handle the real pair's asymmetry (geometric-mean `pi_b_eff`) also gave a
poor match (~44% error) to the real crossing location found in §11.3.

**opus-symbolic-advisor's diagnosis (consulted again, correctly identified the mechanism)**: this
is not a defect of the pure-Gilbert formula specifically -- the boundary LOCATION is inherently
**ill-conditioned**. Since `Phi<=c_warm` is already proven (§ above) and the real dip depth is tiny
(`~0.0006` at the calibrated point), the crossing is where two near-tied curves meet at a shallow
angle -- any perturbation to the inputs can move the crossing far or erase it, independent of which
approximation method is used. **Naive substitution into the pure-Gilbert formula, a Taylor
expansion in `eps` around `(0,1)`, or any other approximation of the crossing location would all
suffer the same fragility** -- this is a property of the TARGET quantity, not of any particular
approximation technique.

**Why Taylor expansion in `eps` specifically doesn't work (a second, independent reason the
advisor gave, confirmed by us)**: the reduction to a finite MDP/semi-Markov structure exists ONLY
at the exact boundary `eps_good=0, eps_bad=1` (deterministic observation collapses belief to a
finite reachable set). For `eps` anywhere else, belief wanders a continuous simplex -- a
CONTINUOUS-belief POMDP. Moving `eps` away from `(0,1)` therefore causes a discontinuous
(singular-perturbation) change in the reachable-belief-set's cardinality (finite -> continuous), so
there is no closed-form Taylor coefficient available at all -- getting even the first-order
correction in `eps` would itself require solving the continuous-belief POMDP, defeating the
purpose of a "simple distilled formula."

**Verification performed (confirms the diagnosis directly, not just by argument)**: traced
`Phi(cost_a)` at several `eps` levels interpolating from pure-Gilbert `(0,1)` toward hop1's real
calibrated `(0.032, 0.301)`. Confirmed the valley does NOT behave pathologically -- it retains
the SAME shape, just gets shallower (a graceful, continuous "parallel shift" upward) until it no
longer dips below zero at all. At the real `eps=(0.032,0.301)`, `Phi` stays pinned at essentially
exactly `c_warm=0.005` from `cost_a=0.02` all the way to `~0.18`, dipping to only `0.004537`
(`90.7%` of the ceiling) at `cost_a=0.20`, never reaching zero in the tested range -- the SAME
underlying dip mechanism as pure-Gilbert, just too shallow now to cross.

**Correct reframe (advisor's proposal, adopted)**: don't try to distill the boundary LOCATION
(fundamentally ill-conditioned, unapproximable by ANY method). Instead, recognize that the
WELL-CONDITIONED, universally-applicable distilled object was ALREADY IN HAND: the proven
`Phi<=c_warm` bound (general theorem above), which uses NO channel-model assumption at all (holds
for pure-Gilbert AND general partial-observation channels identically, since its proof is a pure
policy-domination argument, not a channel-specific calculation). This IS exactly the "simple
formula + rigorously small (here, PROVEN ZERO) error, general engineering takeaway" the user
originally asked for -- just applied to the right target (the gap's ceiling, not the crossing's
location).

**An even stronger empirical finding this reframe motivated** (`warm_cold_wide_range_ceiling_
check_demo.py`): swept `Phi(cost_a)` over a WIDE range (`cost_a` in `[0.02, 0.60]`) for both
hop2 alone and the ACTUAL asymmetric real pair (hop1+hop2, matching `THRESHOLD_PROOF.md` section
6's exact setup). Result: **`Phi` sits at `>=90%` of the proven `c_warm` ceiling across `63.3%`
of the swept range**, and the ONLY place it dips meaningfully below the ceiling is the narrow
window `cost_a in [~0.285, ~0.360]` already identified in section 6 -- and even there, the
deepest point reaches only `-8.1%` of `c_warm` in magnitude (`Phi=-0.000404` at `cost_a=0.300`).
For hop2 ALONE (symmetric), `Phi` sits at EXACTLY `c_warm` across the entire tested range with
only a shallow, non-crossing dip near `cost_a=0.40`. **This is a stronger and cleaner statement
of the real-data negative result than "the operating point sits near a boundary": in the
realistic `eps` regime, cold weakly dominates warm by (very close to) the full `c_warm` amount
across almost the ENTIRE plausible `cost_a` range -- there is barely any boundary/window left at
all for an adaptive policy to exploit, not just a narrow one nearby.**

**A general methodological one-liner worth keeping** (the advisor's phrasing, broadly reusable
beyond this specific problem): when two competing fixed policies are near-tied, the LOCATION of
their crossing is fundamentally ill-conditioned and not robustly approximable by any method, but
the BOUND on their gap is well-conditioned and can be stated universally -- so what is actually
useful for engineering design is not the boundary's exact location, but the bound on the gap.

## CORRECTION: the first phase-diagram/c_warm-scaling tables below had a real bug (retracted, then fixed)

**The tables in this section, as first computed, were WRONG for a large fraction of cells and
have been corrected below.** Bug: the quick "exact" cold-side solver written for this phase-
diagram exploration (`solve_cold` in `warm_win_phase_diagram_pure_gilbert_demo.py` and
`warm_win_c_warm_scaling_demo.py`) only compared the finite-park semi-Markov optimum against ONE
degenerate plateau candidate (`cost_a`, i.e. "always route A forever") -- it omitted the OTHER
degenerate candidate, "always route B forever" (value = the stationary `path_b_loss =
pi_b*(2-pi_b)`), which becomes the TRUE cold-optimal once `cost_a` exceeds that stationary loss
(routing A is then simply a bad deal on average, regardless of channel state, and the semi-Markov
reduction's own state space cannot represent "never return to A" as an option). This caused
`g_cold` to be badly OVERESTIMATED (hence `Phi` underestimated / a fake, much wider-and-deeper
"warm-win window" reported) for every `cost_a` beyond the stationary loss -- which affected most
of the grid, since the naive search range routinely extended well past it.

**Caught via cross-validation against the general (already-trusted) `switching_curves` belief-
grid solver**: while separately checking `eps`-tolerance (below), the general solver's `t=0`
(exact pure-Gilbert) result for `pi_b=0.1, lambda=0.4` showed NO warm-win window at all, directly
contradicting the phase-diagram script's claimed window `[0.688,0.861]` -- a real, reproducible
disagreement (confirmed robust to solver resolution up to 250, ruling out a grid-resolution
explanation on the general-solver side), traced to the missing-candidate bug above by direct
side-by-side comparison of both solvers' `g_warm`/`g_cold` at matching `cost_a` values.

**Fix**: `g_cold = min(finite-park joint-argmin, cost_a, pi_b*(2-pi_b))` (three-way min, not
two-way). Both the phase-diagram and `c_warm`-scaling scripts were corrected and rerun; the
`eps`-tolerance scripts were NOT affected (they always used the general belief-grid solver, which
already handles this correctly via its own full policy space, with no separate "plateau
candidate" bookkeeping needed) -- and their results are now directly consistent with the
corrected phase diagram at `t=0`, as expected.

**This is a significant correction, not a minor refinement**: the earlier (wrong) trend claimed
window depth DECREASES with `lambda` (slower decorrelation favors cold); the CORRECTED trend
shows the OPPOSITE -- window depth INCREASES with `lambda` (see corrected table below), and several
low-`lambda` cells that previously showed the deepest windows now show NO window at all. The
qualitative intuition has been revised accordingly (see "revised intuition" below the corrected
table). Standing methodological lesson (now the fourth occurrence of a numerical-artifact scare in
this thread, previous three all resolution/grid-range related): **cross-check any new,
quickly-written "exact" solver against an already-validated one on at least one shared point
before trusting its output, especially when re-deriving pieces (like a plateau-candidate set) that
were already worked out carefully elsewhere in the project.**

## FULL WARM-WIN PHASE DIAGRAM (2026-07-19, user request: characterize where warm dominates, incl. meta-parameters) -- CORRECTED

Per a direct user request to characterize the FULL warm-win region across meta-parameters (not
just the single calibrated point), built the exact pure-Gilbert phase diagram
(`warm_win_phase_diagram_pure_gilbert_demo.py`): for each `(pi_b, lambda)` at fixed
`c_warm=0.02, c_switch_warm=0.01, c_switch_cold=0.02`, found the FULL warm-win window in `cost_a`
-- both the already-known closed-form LOWER edge and a numerically-found UPPER edge (where `Phi`
returns to `+c_warm` as `cost_a` grows further, both regimes having saturated to always-route-B).

```
pi_b  lambda  cost_a_lo  cost_a_hi   width    min_Phi   depth/c_warm
0.10   0.20      --         --        --        --      no window
0.10   0.40      --         --        --        --      no window
0.10   0.60    0.171      0.440     0.269   -0.0045       22.6%
0.10   0.80    0.134      0.391     0.257   -0.0078       39.2%
0.20   0.20      --         --        --        --      no window
0.20   0.40    0.289      0.481     0.193   -0.0128       64.0%
0.20   0.60    0.208      0.564     0.356   -0.0249      124.5%
0.20   0.80    0.123      0.667     0.544   -0.0296      147.9%
0.30   0.20    0.521      0.521     0.000   -0.0023       11.5%   (degenerate: single point)
0.30   0.40    0.414      0.612     0.197   -0.0240      120.2%
0.30   0.60    0.299      0.727     0.428   -0.0386      193.2%
0.30   0.80    0.176      0.766     0.589   -0.0438      218.8%
0.40   0.20      --         --        --        --      no window
0.40   0.40    0.538      0.691     0.154   -0.0277      138.6%
0.40   0.60    0.393      0.805     0.412   -0.0446      223.1%
0.40   0.80    0.235      0.764     0.529   -0.0506      253.2%
0.50   0.20      --         --        --        --      no window
0.50   0.40    0.663      0.790     0.126   -0.0214      107.0%
0.50   0.60    0.494      0.871     0.377   -0.0423      211.4%
0.50   0.80    0.303      0.821     0.519   -0.0494      246.9%
```

**Note on `depth/c_warm` exceeding 100%**: this is NOT a violation of the proven `Phi<=c_warm`
bound -- that theorem bounds `Phi` only from ABOVE (warm can never beat cold by more than
`c_warm` at the low/high `cost_a` extremes, structurally). It says nothing about how far BELOW
zero `Phi` can go in between, and here it goes moderately below in the deepest cells (up to
`~253%` of `c_warm`, i.e. warm beating cold by up to about 2.5x the probe cost at the very best
point tested) -- much more modest than the withdrawn buggy numbers (which claimed up to `7570%`),
but still a real, meaningful multiple of `c_warm`, not just a hair's-breadth win.

**Corrected qualitative trends** (exact pure-Gilbert, bug fixed) -- **the `lambda` trend direction
is REVERSED from the earlier (wrong) writeup**:
- **Window depth (and its very existence) INCREASES with `lambda`** (slower decorrelation /
  MORE persistence favors warm more, not less) -- e.g. at `pi_b=0.3`: no real window at
  `lambda=0.2` (a single degenerate point), growing to `120%` at `lambda=0.4`, `193%` at
  `lambda=0.6`, `219%` at `lambda=0.8`. Same direction at every `pi_b` tested.
- **Low `lambda` (fast decorrelation) frequently has NO warm-win window at all** (`pi_b=0.1/0.2/
  0.4/0.5` all show "no window" at `lambda=0.2`) -- fast-decorrelating channels are close to i.i.d.
  noise around their own stationary distribution regardless of recent history, so there is little
  persistent structure for continuous monitoring to exploit; a blind guess is nearly as good.
- **Revised intuition**: warm's edge comes from being able to track and exploit PERSISTENT
  stretches of Good state precisely. The more persistent the channel (`lambda` near 1 -- once
  Good, tends to stay Good for a while), the more there is to lose by guessing blindly (cold might
  park through, or return in the middle of, a long favorable or unfavorable stretch) and the more
  continuous monitoring is worth its `c_warm` cost. A fast-decorrelating channel (`lambda` near 0)
  has little such structure to exploit, so warm's information advantage shrinks toward
  insignificant, and the window can vanish entirely.
- **Window depth still increases with `pi_b`** (worse average channels favor warm more) -- this
  part of the original (pre-bug) writeup survives the correction, e.g. at `lambda=0.8`: `39.2%`
  (`pi_b=0.1`) `-> 147.9%` (`0.2`) `-> 218.8%` (`0.3`) `-> 253.2%` (`0.4`) `-> 246.9%` (`0.5`,
  slightly lower, roughly flattening out near `pi_b=0.4-0.5`).
- `cost_a_lo` itself is EXACTLY known in closed form and scales linearly in `c_warm`, but (see the
  corrected `c_warm`-scaling check below) that closed form is only valid within a bounded domain
  -- for `cost_a` beyond the stationary `path_b_loss=pi_b*(2-pi_b)`, cold has already switched to
  "always-B forever," capping the window's upper edge well below what the naive closed form or an
  under-constrained numeric search would suggest.

**`c_warm` scaling check** (`warm_win_c_warm_scaling_demo.py`, at `pi_b=0.3, lambda=0.4`; CORRECTED
after the plateau-candidate bug fix above): swept `c_warm` in `{0.005, 0.01, 0.02, 0.04, 0.08}`
(fixed `c_switch_cold=0.02`):

```
c_warm  cost_a_lo  cost_a_hi   width    min_Phi   depth/c_warm   lo/c_warm  hi/c_warm
0.005    0.3806     0.7431    0.3625   -0.0390        780.0%        76.1      148.6
0.010    0.3919     0.6905    0.2986   -0.0338        338.2%        39.2       69.0
0.020    0.4144     0.6118    0.1974   -0.0240        120.2%        20.7       30.6
0.040    0.4596     0.5033    0.0438   -0.0040         10.1%        11.5       12.6
0.080      --         --        --        --        no window found
```

**Much more sensible than the withdrawn (buggy) table**: the warm-win window now clearly SHRINKS
and eventually VANISHES as `c_warm` grows (nothing found by `c_warm=0.08`), matching the intuitive
expectation that a large enough probe cost eventually cannot be justified by any amount of
channel information -- the opposite of the earlier (bugged) table's implausible finding that the
window kept existing (and widening) even as `c_warm` grew 16x. `depth/c_warm` (the window's
RELATIVE strength) now drops sharply and monotonically with `c_warm` (`780%->338%->120%->10%->
gone`), and `cost_a_hi` is now correctly capped below the stationary `path_b_loss` for this
`pi_b=0.3` pair (`=0.51`) at every `c_warm` tested, consistent with cold's "always-B forever"
option properly limiting how far the window can extend. **Practical takeaway: for fixed channel
parameters, there is a finite `c_warm` ceiling above which NO warm-win window exists at all** (here,
somewhere between `0.04` and `0.08`) -- a genuinely useful, simple design fact: check whether your
system's actual `c_warm/cost_a` ratio falls below this ceiling before considering adaptive/warm
standby worthwhile at all.

**`eps`-tolerance check** (`warm_win_eps_tolerance_v2_demo.py` -- the corrected version; v1 used a
closed-form-centered search grid too narrow to reliably find the true window for several points,
the same class of search-range mistake as the bug above, though this script itself always used the
already-trusted general belief-grid solver so its NUMBERS were fine wherever it searched the right
place, just not always doing so). For representative `(pi_b, lambda)` points, interpolated
`eps_good, eps_bad` from the pure-Gilbert idealization `(0,1)` toward moderately realistic targets
(`eps_good~0.05, eps_bad~0.40`, roughly Berlin-V2X scale), using the CORRECTED phase diagram's own
known `[cost_a_lo, cost_a_hi]` to size the search grid:

```
pi_b  lambda  pure-Gilbert window   tolerates up to (t, fraction toward target eps)
0.1    0.4    no window (t=0 confirms corrected phase diagram)   n/a
0.3    0.2    barely exists (width~0, depth~0.002 at t=0)         t=0.0 (essentially none)
0.3    0.4    depth -0.0216 at t=0                                 t=0.5
```

**These are now fully consistent with the corrected phase diagram** (both show no real window for
`pi_b=0.1,lambda=0.4` and only a knife-edge one for `pi_b=0.3,lambda=0.2`, matching the "low
`lambda` -> shrinking/vanishing window" corrected trend above) -- confirming the general solver
was reliable throughout; only the quickly-written "exact" pure-Gilbert reimplementation had the
missing-plateau-candidate bug. **Directionally, real-world `eps` blur (departure from the
idealization) further shrinks an already-`lambda`-and-`pi_b`-dependent window** -- for the one
point with a comfortably real interior window at `t=0` (`pi_b=0.3, lambda=0.4`), only about half
the distance toward realistic `eps` values could be tolerated before the window closed entirely.

## DEEP-DIVE (2026-07-19, advisor-guided): literature check on the lambda-trend + closed form for the c_warm ceiling

Per user request, went one level deeper on two follow-ups to the corrected phase diagram above,
consulting `opus-symbolic-advisor` throughout.

### (1) Is "monitoring value increases with persistence" already known? Yes -- cite, don't claim as novel.

Both an independent web-search literature agent and the advisor's own search converged on the
same honest verdict: **the qualitative direction (continuous monitoring's value over blind
operation grows as the underlying Markov chain's persistence/autocorrelation increases, and
vanishes as the chain approaches i.i.d.) is already established** in the opportunistic-spectrum-
access / restless-bandit-over-Gilbert-Elliott literature -- most directly **Peng, Low & Blanch (or
similar), "Optimal Power Allocation over Two Identical Gilbert-Elliott Channels" (arXiv:1210.3609)**
(a TWO-Gilbert-Elliott-channel structure, closely matching this project's own 2-hop setup) and
**Zhao/Krishnamachari/Liu, "Myopic Sensing for Multi-Channel Opportunistic Access" (arXiv:
0712.0035)**, which classically formalize how positive correlation (persistence) governs the value
of sensing/exploiting a channel. **Important caution from the advisor, confirmed independently**:
do NOT cite plain Age-of-Information (AoI) literature as precedent here -- AoI's own monotonicity
direction is the OPPOSITE (fast-changing sources need MORE frequent refresh, not less), so it is
not the right family of prior art for this specific qualitative direction, even though AoI was the
correct prior art for the EARLIER `pi_b>1/2` single-component threshold finding (§ above). **Given
this, the qualitative direction is NOT this project's contribution -- the contribution must be
placed on the QUANTITATIVE closed-form characterization (below), not the qualitative trend.**
Also flagged as directly relevant: Kumar et al.-style "MDP with observation costs" framings
(arXiv:2201.07908) and time-correlated-channel remote-estimation scheduling (arXiv:2303.16285,
2403.13898) for the general value-of-information framing this project's `Phi` bound instantiates.

### (2) Closed form for the c_warm ceiling -- three-part answer (exact value + leading-order form + a general envelope characterization), NOT a single closed form

**Key structural fact (advisor's insight, confirmed)**: since the warm regime pays `c_warm` every
step REGARDLESS of routing policy, `c_warm` enters `Phi` PURELY ADDITIVELY across every active
cell: `Phi(cost_a; c_warm) = c_warm + Psi(cost_a)`, with `Psi` entirely `c_warm`-independent. Two
consequences, verified:

**(i) The window's depth is EXACTLY LINEAR in `c_warm`** (`depth(c_warm) = c_warm_vanish -
c_warm`), so `c_warm_vanish` (the value above which the window ceiling vanishes) can be read
directly off ANY single data point via `c_warm_vanish = c_warm + depth` -- no need to re-sweep
`c_warm` at all. **Verified numerically to 4 significant figures** across all 4 tested `c_warm`
values from the corrected `c_warm`-scaling table (`pi_b=0.3, lambda=0.4, c_switch_warm=0.01,
c_switch_cold=0.02`): `0.043999, 0.043825, 0.044039, 0.044033` -- all converge on
`c_warm_vanish ~= 0.0440`. **Independently cross-checked** by directly computing `Psi(cost_a)` at
a near-zero `c_warm` probe value and finding its numeric minimum directly: valley bottom at
`cost_a~=0.456`, `Psi_min=-0.044127` -- matching the data-driven estimate almost exactly (two
fully independent methods agreeing to within `~0.0002`).

**(ii) A leading-order closed form exists, but is a LOWER BOUND, not exact.** Within the primary
active cell (`warm=route-B-iff-GG, cold=always-A-cold plateau`), `Psi` is EXACTLY LINEAR in
`cost_a` (`Psi = a^2*K - a^2*cost_a`, `a^2=(1-pi_b)^2`, `K=(1-q_G^2)*(1+2*c_switch_warm)`) --
verified symbolically (`sp.diff` gives exactly `-a^2`, no curvature at all within one cell). So the
window's "dip and return" shape comes ENTIRELY from CELL TRANSITIONS as `cost_a` grows (not from
any interior curvature within a single cell) -- crossing into cold's finite-park regime, then
eventually both regimes saturating to always-B. Using the naive candidate
`cost_a_boundary = min(cold's finite-park detachment threshold, cold's own always-B threshold,
warm's own route-B-iff-GG-to-always-B threshold)` gives `c_warm_vanish_leading = a^2*
(cost_a_boundary - K)`. Numerically, at the test point this gives `~0.0404` -- in the RIGHT
ballpark and correct order of magnitude, but **short of the exact `0.0440` by about 8%,
confirmed to be a real gap, not noise.**

**(iii) The 8% gap has a clean explanation: a general envelope-theorem characterization of the
valley bottom, verified numerically.** `dPhi/dcost_a = (A-dwelling fraction under warm's optimal
policy) - (A-dwelling fraction under cold's optimal policy)` by the envelope theorem (each
optimal-policy value's cost_a-derivative equals its own policy's realized A-fraction, holding the
policy's own free parameters at their optimum) -- a fact that holds for ANY active cell, ANY
channel model, not just pure-Gilbert. **The valley bottom (Phi's minimum, i.e. warm's maximal
advantage point) therefore occurs EXACTLY where the two optimal policies' A-dwelling fractions
coincide.** In the primary cell, warm's own A-fraction is fixed at `b^2 = pi_b*(2-pi_b)` (routes to
A whenever NOT in the jointly-Good state). **Verified directly**: tracking cold's OWN optimal
`(n_H*, n_BB*)` and its resulting A-dwelling fraction across the discrete transitions near the
valley (`pi_b=0.3, lambda=0.4`): `A_frac_cold` jumps from `0.5596` (at `n_H*=4`) to `0.4835` (at
`n_H*=3`) exactly at `cost_a=0.456` -- and **`b^2=0.51` falls precisely BETWEEN these two
straddling values, with the discrete valley bottom sitting exactly at that same transition point**.
This confirms the continuous-relaxation envelope condition governs the true (integer-constrained)
valley bottom almost exactly, and explains WHY the naive leading-order estimate undershoots: right
at cold's detachment threshold, cold's own A-fraction is still close to `1` (park length still
effectively unbounded there), far above `b^2` -- the true valley lies further into the finite-park
cell, where cold's A-fraction has had room to fall all the way down to (bracket) `b^2`.

**Why NOT to chase a single exact closed form further (advisor's recommendation, adopted)**: the
envelope condition `A_frac_cold(cost_a) = b^2` is only an IMPLICIT condition, since
`A_frac_cold(cost_a)` depends on cold's own optimal `n*(cost_a)` -- which (per the extensive
`pi_b>1/2` investigation earlier in this document) does not have a clean closed form in general
(transcendental/fold-at-infinity structure). Solving the envelope condition exactly would require
reopening that whole machinery for an 8% correction -- exactly the kind of "monster closed form for
marginal gain" this project's own standing principle (state a finite/numeric algorithm honestly
rather than force an ugly closed form) argues against.

**Final three-part characterization adopted for the paper** (no single closed form, by design,
stated as such): (a) the EXACT numeric ceiling value, obtained essentially for free via the linear
law `c_warm_vanish = c_warm + depth` from any single computed point (rigorous, cheap, 4-digit
verified); (b) a LEADING-ORDER closed form `c_warm_vanish ~= a^2*(cost_a_boundary - K)` giving the
`(pi_b, lambda)` DEPENDENCE explicitly, including the clean limit `lambda->1 => K->0 =>
c_warm_vanish -> a^2*cost_a_boundary` (its maximum) -- i.e. the ceiling strictly rises as
persistence increases, a quantitative sharpening of the qualitative fact from (1); (c) a GENERAL,
channel-model-independent ENVELOPE CHARACTERIZATION of exactly where and why the true valley
bottom sits relative to the leading-order estimate (the two policies' fallback-path usage
fractions coincide there) -- itself a non-obvious structural insight not reducible to (1)'s prior
art, and the genuine novel contribution of this sub-investigation.

## SYSTEMATIC ERROR QUANTIFICATION OF THE LEADING-ORDER CLOSED FORM (2026-07-19, user request)

Per direct user request, quantified the leading-order closed form's approximation error
systematically across the FULL `(pi_b, lambda)` phase-diagram grid
(`c_warm_vanish_approximation_error_demo.py`), not just the single `(pi_b=0.3, lambda=0.4)` point
checked during its derivation (where it undershot by `~8%`). **A real bug was caught and fixed
while building this check**: the first version searched for the true valley bottom using a
`cost_a` range derived from the `c_warm_vanish` ESTIMATE itself (a completely different
unit/scale from `cost_a`), producing nonsensical negative "true" values across nearly the whole
grid -- fixed by anchoring the search range on `cost_a_boundary` (the actual `cost_a`-scale
candidate) instead, per the same "always search in the right units/range" lesson this thread has
hit several times before.

**Corrected full-grid result** (`c_switch_warm=0.01, c_switch_cold=0.02`):

```
pi_b  lambda  leading_order  true(numeric)  rel_err%
0.10   0.20     0.007376       0.010066      26.7%
0.10   0.40     0.009676       0.018016      46.3%
0.10   0.60     0.009566       0.023707      59.6%
0.10   0.80     0.006547       0.018911      65.4%
0.20   0.20     0.017525       0.019212       8.8%
0.20   0.40     0.026203       0.032815      20.2%
0.20   0.60     0.028556       0.045173      36.8%
0.20   0.80     0.021458       0.046504      53.9%
0.30   0.20     0.024806       0.025261       1.8%
0.30   0.40     0.040455       0.043980       8.0%   (matches the original single-point check exactly)
0.30   0.60     0.047902       0.058796      18.5%
0.30   0.80     0.039642       0.063774      37.8%
0.40   0.20     0.026833       0.025981      -3.3%
0.40   0.40     0.047025       0.047955       1.9%
0.40   0.60     0.060331       0.064804       6.9%
0.40   0.80     0.055568       0.070677      21.4%
0.50   0.20     0.023766       0.023056      -3.1%
0.50   0.40     0.044424       0.043761      -1.5%
0.50   0.60     0.061643       0.062274       1.0%
0.50   0.80     0.063823       0.069407       8.0%
```

**Summary statistics**: mean relative error `20.8%`, median `13.7%`, range `[-3.3%, +65.4%]` --
**the error is emphatically NOT uniform** across the meta-parameter space, and the original
single-point check (`8%`, at `pi_b=0.3,lambda=0.4`) happened to land in a moderate part of the
range, not representative of the worst case.

**Clean, honest pattern**: error is SMALLEST (a few percent, sometimes even a small negative --
i.e. the leading-order form is NOT strictly a lower bound after all, just approximately one) when
`pi_b` is LARGE and `lambda` is SMALL-TO-MODERATE (best: `pi_b=0.4-0.5, lambda=0.2-0.4`, errors
`-3.3%` to `1.9%`). Error is LARGEST (up to `65%`) when `pi_b` is SMALL and `lambda` is LARGE
(worst: `pi_b=0.1, lambda=0.8`). **Mechanistic explanation, tying back to the envelope
characterization above**: the true valley bottom is where cold's own `A`-dwelling fraction has
fallen from near-`1` (at its naive detachment point) down to warm's fixed value
`b^2=pi_b*(2-pi_b)`. When `pi_b` is small, `b^2` is itself small (far below `1`), so cold's
`A`-fraction must fall a LONG way, requiring a large excursion into the finite-park cell in
`cost_a`-space -- stretching the gap between the naive (detachment-point) estimate and the true
valley. When `pi_b` is large, `b^2` is closer to `1`, so only a small fall is needed, and the naive
estimate is nearly exact. Higher `lambda` (more persistence) similarly stretches the transition
further (consistent with the phase diagram's finding that persistence deepens and widens the
window generally), compounding the error in the same direction.

**Bottom line for engineering use**: the leading-order closed form is a genuinely USEFUL,
order-of-magnitude/qualitative-dependence tool (correct `(pi_b,lambda)` trend direction and clean
`lambda->1` limit everywhere tested) but should NOT be trusted for a precise numeric answer,
especially in the `pi_b<=0.2` region where errors of `30-65%` are common. **The exact numeric value
(via the linear law, `c_warm_vanish = c_warm + depth`, free from any single computed point) should
always be used when a real number is needed**; the leading-order form is for understanding WHY the
ceiling moves the way it does, not for computing it precisely.

## ADVISOR CORRECTION AND UNIFICATION (on n_H/n_BB thresholds)

Advisor's own follow-up retracted their "n_BB won't have a clean form" claim (the closed form
above, verified numerically by me, was correct) and gave a clean unification worth recording:

```
cost_a*(entry) = (1 + 2*c_switch_cold) / (1 + N*P_GG^max(entry))
```
with `P_GG^max` = `(1-pi_b)/(4*pi_b)` for H (bump vertex, `pi_b>1/2` only) or `(1-pi_b)^2` for BB
(monotone saturation limit, all `pi_b`). And a clean PROOF that `cost_a*_H < cost_a*_BB` always
holds when `pi_b>1/2` (equivalent to `4*pi_b*(1-pi_b)<=1`, true for all `pi_b`, with equality only
at `pi_b=1/2`) -- so the "H detaches before BB" ordering (once correctly measured, per the
resolution-trap correction above) is a GUARANTEED consequence of the bump, not a coincidence of
the specific parameter point tested.

**Crucial simplification for the DEFAULT real-data case (`pi_b<=1/2`)**: H's vertex `x*=(2*pi_b-
1)/(2*pi_b)` becomes NEGATIVE (invalid) for `pi_b<=1/2`, so `P_GG_H` loses its bump and becomes
monotone too -- and BOTH entries then saturate to the SAME asymptotic value `(1-pi_b)^2` (each hop
independently reaches its own stationary `(1-pi_b)` marginal). **Consequence: for `pi_b<=1/2`,
H and BB share the exact same detachment threshold, the two-stage structure and interior-optimum
window both disappear entirely, and cold-side behavior collapses to a simple binary**: below
threshold, `g_cold*=cost_a` (plateau/always-A-cold); above it, a simple (non-multimodal, safely
first-difference-searchable) finite park duration. `pi_b=1/2` is the exact structural watershed.
This is the mental model priority (3) below builds on.

## PRIORITY (3): Phi=0 BOUNDARY -- FIRST CLOSED FORM FOUND (primary/default active cell)

Per the advisor's recommended approach (trace `Phi=0` numerically first using BOTH sides' exact
finite algorithms -- warm via the validated 8-state policy iteration, cold via the joint
`(n_H,n_BB)` argmin-with-plateau-candidate -- identify which policy pair is ACTIVE at the
boundary, then derive a closed form only for that cell, not a monster covering every regime pair):
new script `warm_cold_phi_zero_active_cell_demo.py` sweeps `cost_a` for a `pi_b<=1/2` point
(`pi_b=0.3, lambda=0.4, c_warm=0.02, c_switch_warm=0.01, c_switch_cold=0.05`) and reports both
sides' exact optimal cost and active policy at each point.

**Clean anchor confirmed exactly** (low `cost_a`, both sides degenerate to "always-A"): `Phi =
g_warm* - g_cold* = c_warm` EXACTLY (`0.020000` at every tested point in this regime, matching
`c_warm=0.02` to all 6 printed decimals) -- confirms the advisor's predicted mechanism: when
neither side ever benefits from routing to B, cold strictly wins by exactly the warm probe's
per-step cost, with no other structure. Trivial but a useful, exactly-verified sanity anchor.

**The actual `Phi=0` crossing happens in a clean, simple active cell**: as `cost_a` increases, WARM
switches first (at `cost_a~0.35`) from `always-A` to `route-B-iff-GG`, while COLD is still in
`plateau(always-A-cold)` -- and it is in THIS cell (`warm=route-B-iff-GG`, `cold=plateau`) that
`Phi` crosses zero (observed between `cost_a=0.3686` [`Phi=+0.0031`] and `0.3873` [`Phi=-0.0061`]),
well BEFORE cold itself later transitions to `finite-park` (`cost_a~0.44`) -- i.e. **cold's own
finite-park regime is irrelevant to where this boundary actually sits**; only cold's plateau value
(`g_cold*=cost_a`, trivial) and warm's `route-B-iff-GG` closed form are needed.

**Closed form for this active cell** (substituting the two already-validated pieces --
`g_warm(route-B-iff-GG) = c_warm + a^2*(1-q_G^2)*(1+2*c_switch_warm) + (1-a^2)*cost_a` where
`a=1-pi_b, q_G=1-p_gb=1-pi_b*(1-lambda)`, and `g_cold(plateau)=cost_a` -- and solving `Phi=0` for
`cost_a`):

```
cost_a* = c_warm/(1-pi_b)^2 + (1 - q_G^2) * (1 + 2*c_switch_warm)
```

**Verified to 5 decimal places** against the exact 8-state warm policy-iteration solver: closed
form gives `cost_a*=0.3749683265`; fine bisection of the exact solver shows `Phi` flipping sign
symmetrically between `cost_a=0.374868` (`Phi=+0.000049`) and `0.375068` (`Phi=-0.000049`),
bracketing the closed form almost exactly at its midpoint (see
`verify_phi_zero_precise_tmp.py`-style check, reproduced in
`warm_cold_phi_zero_active_cell_demo.py`'s docstring math). This is the first genuine, validated
closed-form answer to "where is the warm/cold boundary" for the primary (real-data-relevant,
symmetric, `pi_b<=1/2`) case.

**Domain validity** (checked numerically, not yet derived symbolically): this closed form is only
valid while (a) `route-B-iff-GG` is genuinely warm-optimal at `cost_a=cost_a*` (true in the tested
point, `warm` already switched at `cost_a~0.35 < cost_a*~0.375`), and (b) `cost_a* <
cost_a*_cold(detachment)` so cold is still in its plateau there (`cost_a*_cold=(1+2*c_switch_cold)
/(1+N*(1-pi_b)^2)~0.4407` in this example, comfortably above `cost_a*~0.375`) -- both hold at the
tested point but have not yet been turned into general symbolic conditions on `(pi_b,lambda,
c_warm,c_switch_warm,c_switch_cold)`.

**Open next steps**: (1) derive the symbolic validity conditions for (a)/(b) above, to know the
full region of `(pi_b,lambda,...)` space where this specific closed form applies (vs. where a
DIFFERENT active cell -- e.g. `always-B` warm, or `finite-park` cold -- takes over); (2) compare
this closed form's `cost_a*` against the project's real calibrated operating points (Berlin V2X,
though note hop1!=hop2 there so the symmetric closed form only applies approximately/per-hop) to
place the "why does real data show <1%, well below the 5% bar" finding in these terms, per the
advisor's suggested connection back to `THRESHOLD_PROOF.md`/paper (task #5). Continue consulting
opus-symbolic-advisor.

## REAL-DATA Phi POSITIONING -- DECISIVE RESULT (advisor's recommended "(b) first" ordering)

Per the advisor's strong recommendation (real channels have `eps` far from `0/1` and `hop1!=
hop2`, so the pure-Gilbert closed form is an ANALYTICAL ANCHOR, not a literal predictor -- the
real question must be answered by the project's existing GENERAL solver, which already handles
asymmetric partial-observation channels directly, not by force-fitting the symmetric pure-Gilbert
formula onto real data), ran `warm_cold_phi_zero_real_data_position_demo.py`: sweeps `cost_a`
through `switching_curves.always_warm_value_iteration` / `always_cold_value_iteration` directly on
the REAL Berlin V2X EM-fitted hop parameters (hop1 `p_gb=0.1909,p_bg=0.4553,eps_good=0.032,
eps_bad=0.301`; hop2 `p_gb=0.2764,p_bg=0.3933,eps_good=0.0695,eps_bad=0.4253`), at this project's
peak-gain operating point (`c_warm=0.005, c_switch_warm=0.01, c_switch_cold=0.02`, matching
`TRACE_CALIBRATION_NOTES.md`'s "peak relative value 0.65%" point).

**Result: the calibrated real `cost_a=0.30` sits almost exactly ON the Phi=0 crossing.** Coarse
sweep already showed `Phi=-0.000404` at `cost_a=0.30` (`|Phi|/cost_a=0.135%`); a refined sweep
(resolution=100, step=0.0025) resolved the full shape: **the "warm-fixed-policy wins" window is
only `cost_a in [~0.2975, ~0.3075]` wide** (about `0.01` in `cost_a`, i.e. ~3% of its value), with
the deepest point of the dip (`Phi~-0.00057`) at `cost_a~0.302-0.303` -- and `cost_a=0.30` (the
independently-calibrated real value, NOT tuned to hit this) lands almost exactly inside this
narrow dip, near its minimum. Outside this tiny window on either side, `Phi` returns quickly to
`+0.005 = c_warm` EXACTLY (both very low and very high `cost_a` limits: at low `cost_a` both sides
degenerate to always-A, at high `cost_a` both saturate to always-B, and in both extremes cold
simply saves the warm probe's `c_warm` -- the same clean anchor identity found in the pure-Gilbert
analysis, confirmed here in the fully general/asymmetric/partial-observation setting too).

**This is a clean, quantitative, analytical explanation for the earlier applied finding** ("peak
adaptive-decomposition gain 0.65%, below the informal 5% worth-it bar"): the real calibrated
operating point does not sit deep in a regime where one fixed policy dominates the other (which
would leave lots of room for a smarter adaptive combination to win big) -- it sits almost exactly
AT the crossover where the two fixed policies are already nearly tied (`|Phi|/cost_a` well under
1%), in an extremely narrow window. There is very little performance gap between the two SIMPLEST
possible strategies (always-warm, always-cold) for this project's real, calibrated parameters to
begin with, which puts a tight structural ceiling on how much any adaptive policy sitting between
them could possibly add. This gives the earlier real-data conclusion a precise analytical anchor
rather than leaving it as a bare empirical number.

## ADVISOR PRE-PAPER REVIEW: 4 points addressed before this becomes a load-bearing claim

Before writing this into `THRESHOLD_PROOF.md` as a paper-level claim, the advisor flagged four
things to nail down first (all addressed, see `THRESHOLD_PROOF.md` §11.3 for the final wording):

1. **Phi (fixed-vs-fixed) and adaptive gain (adaptive-vs-best-fixed) are DIFFERENT quantities --
   don't conflate them.** A near-tie between the two fixed policies does not, by itself, prove the
   adaptive gain must be small (adaptive policies are a strictly richer class; in principle they
   could still beat both fixed policies widely even when those two are tied with each other). The
   rigorous claim is the DIRECTLY MEASURED adaptive gain (already computed via
   `beliefgrid2d.belief_grid2d_value_iteration_warm` against `min(g_warm,g_cold)`, matching
   `berlin_v2x_block_fit_demo.py`'s `gain()`); `Phi=0` is explanatory context for WHY the gap is
   small, not a proof that it must be.

2. **Robustness sweep** (`warm_cold_robustness_sweep_demo.py`): perturbed `cost_a` and all 4
   parameters of both hops (`p_gb,p_bg,eps_good,eps_bad`) independently by `+/-20%`, 80 samples,
   fixed seed `20260719`, measuring BOTH `Phi` and the true adaptive gain at every point (not just
   `Phi`). **Result: robust.** Adaptive gain stayed `<5%` in ALL 80 samples (`min=-0.000%`
   [numerical noise near zero], `max=0.756%`, `mean=0.087%`, vs. `0.649%` at the unperturbed base
   point) and `|Phi|` never exceeded `c_warm=0.005` in any sample (`53.8%` of samples had
   `|Phi|<c_warm` strictly; every sample satisfied `|Phi|<=c_warm`, consistent with the two-tail
   `Phi->c_warm` anchor identity bounding it from outside the narrow dip). **This rules out the
   "exactly on the boundary" landing being a fragile coincidence** -- reframed the claim (per the
   advisor's specific wording suggestion) from "the point lands exactly on a knife-edge boundary"
   (which reads as numerology) to "**the point sits in a neighborhood that stays robustly
   low-gain under calibration uncertainty**" -- the single-point "almost exactly on Phi=0"
   observation is kept as a striking illustration, but the paper's actual claim rests on the
   whole-neighborhood robustness result.

3. **State the physical driver as a general, conditional design principle, not a one-off number.**
   The entire picture is driven by `c_warm/cost_a` being small (`~1.7%` at the calibrated point,
   directly traceable to task #2/#3's real QUIC path-validation timing calibration showing the
   warm probe is cheap). General statement now in `THRESHOLD_PROOF.md`: whenever `c_warm/cost_a`
   is small for a given deployment, the adaptive gain over the better fixed policy is structurally
   bounded to be small too (visible directly in the `Phi=c_warm` anchor identity and the `cost_a*`
   closed form's `c_warm/(1-pi_b)^2` term) -- a deployment with a substantially more expensive
   warm-probe mechanism would NOT be covered by this conclusion. States the negative result as
   conditional or, on this project's real cost structure, not universal -- reads as a general
   design principle rather than a Berlin-V2X-specific artifact.

**Advisor's follow-up "polish" round (2 more points, both addressed)**:

**Polish 1 (done, then corrected by review): pair the empirical robustness with a structural
reason "why 80/80".** `Phi<=c_warm` is not an empirical accident -- it's a general, provable
domination bound: replaying cold-optimal's exact decision sequence under the warm setup (ignoring
the extra live-probe info) is a FEASIBLE warm policy. **First draft of this argument had a bug,
caught in review**: it assumed the replay's switching cost equals `g_cold*`'s own, but switching
cost is a property of the ENVIRONMENT (warm/cold), not the policy -- a switch in the replayed
sequence is charged `c_switch_warm`, not `c_switch_cold`, even though the decision to switch was
copied from cold's policy. Correct accounting: same decision rule means identical expected routing
loss AND identical switch RATE, but the switch COST differs, giving `replay value = g_cold* +
c_warm - (switch rate)*(c_switch_cold-c_switch_warm)`, hence `Phi <= c_warm - (switch
rate)*(c_switch_cold-c_switch_warm)`. **This is `<=c_warm` exactly when `c_switch_cold>=
c_switch_warm`** -- true physically always here (cold's switch includes warm-up/reconnection
overhead warm's switch, already primed, doesn't pay; every calibrated value in this project
satisfies it, `c_switch_warm=0.01 < c_switch_cold in {0.02,0.10,0.5}` throughout
`TRACE_CALIBRATION_NOTES.md`). **Bonus this correction surfaced**: it also explains tightness --
equality (`Phi=c_warm` exactly) holds precisely when the switch RATE is 0 (the degenerate
always-A/always-B anchors), and `Phi` sits strictly below `c_warm` whenever any switching actually
occurs -- exactly matching why the robustness sweep's `max(Phi)` sat precisely at `c_warm` in the
no-switching-needed samples. This is now stated correctly in `THRESHOLD_PROOF.md` §11.3 as the
structural half of a two-part story: (i) `Phi<=c_warm` whenever `c_switch_cold>=c_switch_warm`
(proven, general, physically satisfied here), (ii) the dip below zero is shallow here because
`c_warm/cost_a` is small (the physical-driver argument, point 3 above), (iii) both survive
perturbation empirically, including at bootstrap-derived widths (the robustness sweep, see below).
Robustness is explained, not just observed -- and the theorem is now airtight, not merely
plausible.

**Polish 2 (done, and it caught a real issue): check `+/-20%` against actual calibration
uncertainty, not assert it.** Ran `berlin_v2x_bootstrap_ci_demo.py`: block bootstrap (block=20
windows, matching this project's own already-established autocorrelation decay scale) over each
hop's per-epoch window sequence, 8 resamples per hop, refitting the Binomial-HMM EM (6
multistarts, fewer than the main fit's 30, for speed). **Result: `+/-20%` was NOT conservative** --
bootstrap std/point ratios ran up to `28.8%` (hop2 `eps_good`, with a full 8-sample range of `95%`
of its point estimate) and `11.7%-14.5%` for both hops' `p_gb`; only `p_bg`/`eps_bad` were
comfortably inside `20%`. Rather than quietly keeping the narrower, unjustified box, re-ran the
robustness sweep (`warm_cold_robustness_sweep_v2_bootstrap_calibrated_demo.py`) with per-parameter
widths set to `~2*(bootstrap std/point)` (floored at the original `20%`, so strictly wider) --
up to `+/-57.6%` for hop2's `eps_good`. **The finding held, and if anything strengthened**: 80
fresh samples, adaptive gain `<5%` in `100%` of them, and in fact `<1%` in `100%` of them
(`max=0.885%`, `mean=0.097%`, `median=0.000%` -- median exactly 0, meaning most perturbed points
land cleanly on one side or the other with essentially zero measurable adaptive advantage at all).
This is a more honest AND a stronger result than the original flat-`20%` sweep: the low-gain
conclusion is robust to REAL (bootstrap-measured), not merely assumed, calibration uncertainty --
worth recording as a general methodological point: **when asserting a perturbation width represents
"calibration uncertainty," always ground it in an actual uncertainty estimate (bootstrap, CI from
the fitting procedure, etc.) rather than picking a round number and calling it conservative.**

4. **Peer review via independent re-execution, not self-report** (per this project's own
   established lesson, `[[peer-review-execute-not-just-read]]`): before treating any of the above
   numbers as settled, spawn an independent agent with NO access to this session's transcript to
   re-run the real-data comparison from scratch and report its own numbers, to catch any
   self-verification blind spot before this goes into the paper.

   **Done**: a fresh agent (Fable, isolated worktree, zero prior context) wrote its OWN script
   (not a re-run of this project's demo files) against the same real hop parameters and
   independently got `g_warm=0.29917536, g_cold=0.29957952, Phi=-0.00040417,
   g_adaptive=0.29720598, gain=0.6583%` -- both claims (`Phi` in `[-0.0006,-0.0003]`, `gain` in
   `[0.5%,0.8%]`) verified. **One genuine finding from this independent check**: it additionally
   tested resolution sensitivity (40 vs. 80) and found `Phi` itself is fairly resolution-sensitive
   (`-0.00031` to `-0.00054`, ~1.7x spread) -- so `Phi`'s precise numeric value should be quoted
   only to 1 significant figure / as an order-of-magnitude explanatory quantity, not as a precise
   landing coordinate. The **gain** figure (the actually load-bearing quantity) was stable across
   resolutions (`0.6375%`-`0.6588%`) -- reinforcing point 1 above (gain is the rigorous number;
   `Phi` is explanatory context, and now also shown to be the less numerically stable of the two,
   which is a further reason not to lean on its precise value in the paper).

**Domain restriction, flagged by the advisor (record explicitly, easy to forget)**: the symbolic
formula's `n_entry` (= the traced decay exponent) is only physically meaningful for
`n_entry >= 2` (i.e. park-step count `n_entry - 1 >= 1`, at least one real park step). `n_entry=1`
would mean "retreat then immediately return with zero park steps," which is not a real transition
in the underlying MDP (once you choose to switch to A, you stay there at least one full step by
construction) -- charging `2*c_switch_cold` for a same-instant B->A->B "phantom switch" at
`n_entry=1` would be meaningless. Both validated benchmark points (`n_H=3,n_BB=4` and
`n_H=2,n_BB=3`) satisfy `n_entry>=2`, so this restriction didn't bite in this session's checks,
but must be respected when later doing the `n*` optimization (minimize over integers `n>=2`,
treating `n=1`-equivalent "never retreat, ride through the bad state" and `n->infinity`
"permanent park" as separate degenerate branches to compare against, not as limiting cases of
this formula itself).

**Two invariants worth stating explicitly as self-checks for any future re-derivation** (both
already confirmed to hold in this session's symbolic construction, per direct advisor
cross-check): (a) the per-cycle B-phase loss is EXACTLY 1 regardless of belief, because a cycle
contains exactly one retreat-triggering non-GG step (loss=1) and all other B-phase steps are
GG (loss=0) -- this is what forces the `R(H)=R(BB)` simplification above, not a coincidence; (b)
the embedded transition matrix's rows sum to exactly 1 automatically whenever the `f_GG`
exit-distribution weights are properly normalized (`f_GG_H + f_GG_BB = 1`, already checked
symbolically in the script's own output) -- if a future edit breaks either invariant, that is the
first thing to check before trusting any new numeric substitution.

## Recommended framing if this is written up NOW (updated after the sympy session)

"Established and validated the reduction theorem on both sides (finite MDP for warm, semi-Markov
renewal for cold -- the latter confirmed structurally via Monte Carlo, not just plausibility).
**Fully derived and machine-precision-validated explicit closed-form formulas for g_warm** in the
symmetric-hop case (three named policies: always-A, always-B, route-B-iff-GG), triple-checked
against an independent hand derivation and the numerical solver. **The analogous closed form for
g_cold is NOT yet correct** -- three iterations of the semi-Markov cycle-accounting narrowed the
error from 0.080 to 0.011 (of a true value ~0.174) but did not eliminate it, and a from-scratch
Monte Carlo simulation of the same policy structure independently confirms the qualitative
renewal-cycle hypothesis is right while showing the exact analytical formula still has a
bookkeeping bug. This is an honest, substantive, PARTIAL answer: the warm side is genuinely done;
the cold side needs one more focused debugging pass (a concrete comparison against the working
Monte Carlo reference, not a fresh re-derivation from theory) before its closed form can be
trusted."

## "FRICTION ZONE" INVESTIGATION (2026-07-19): right-sized to a crossover-width remark, not a standalone 3-way classification

User proposed a genuinely interesting new direction: instead of a binary cold-wins/warm-wins
split, formalize a THREE-WAY classification (cold-dominant / a "friction/indifference zone" where
cold's optimal park length hasn't resolved / warm-dominant), motivated by the semantic
observation (in dialogue, not just computation) that the leading-order approximation's error
seemed to reflect a genuine "no one has committed to a strategy yet" region near cold's
detachment threshold, not just a smooth approximation gap.

**Initial numeric support looked strong but was partly an artifact -- caught via the advisor's
insistence on re-checking against a wider search range (the fourth time in this thread a
"argmin stuck at the search boundary" reading turned out to be under-resolved, not genuine)**:
at `pi_b=0.3, lambda=0.8`, just `0.1%` above the detachment threshold, the optimal park length
`n^*` appeared "stuck" at a search ceiling of `30` -- but widening the ceiling to `10,000` revealed
the TRUE optimum was a finite `n^*=44`, not diverging/unresolved at all. **However, the underlying
signal survived the correction**: measuring `Delta_g/depth` (the ratio of the objective's
variation across a `3x` window of candidate park lengths around the optimum, to the optimum's own
depth below the "always-A" plateau) showed a real, consistent, ~`10x` difference between
`lambda=0.8` and `lambda=0.2` at matched distances from detachment (`154` vs `15` at `0.1%` past
detachment; `22` vs `3.1` at `2%`; `3.3` vs `0.33` at `20%`) -- i.e. the near-optimum genuinely IS
much flatter (relative to its own depth) for high-persistence channels, even though it's not
literally indeterminate.

**Literature check (independent web-search agent)**: the general SHAPE of this phenomenon
(objective value well-determined/smooth through a fold-type transition, while the optimal control
is comparatively flat/non-unique near it) is close to folk-theorem-level knowledge in several
adjacent fields -- the classical Sethi/Skiba "DNSS point" literature in optimal control (value
continuous, optimal trajectory non-unique at an indifference point), and the well-documented
ill-definedness of Whittle indices near restless-bandit indexability boundaries. No source was
found stating the SPECIFIC compound claim (a quantified flatness ratio, tied to the underlying
Markov chain's persistence, in this exact park-duration/renewal-reward setting) -- but the
qualitative shape is not novel, matching the pattern seen repeatedly elsewhere in this
investigation (§ above): specific quantitative instantiation of a known general phenomenon.

**Final scoping decision (advisor consultation, full agreement)**: do NOT pursue this as a
standalone 3-way classification / phase diagram. Three reasons, all independently confirmed:
1. **It's a continuous crossover, not a genuine third "phase"** -- `n^*` is finite and
   well-determined throughout (once properly resolved), just varying in how SHARPLY. Calling it a
   distinct "indifference regime" would be an overclaim the data doesn't support; "the cold->warm
   boundary's crossover WIDTH grows with persistence" is the accurate description.
2. **It doesn't affect the applied VALUE/DECISION question**: throughout this zone, `g_cold` is
   still near the plateau value and warm does not win there -- the ambiguity is entirely in the
   POLICY (which exact park length cold "should" use), not in whether warm or cold is better. The
   applied question this whole project cares about (is adaptive/warm worth it) is unaffected.
3. **It is a direct corollary of already-established results** (the fold-type detachment + the
   persistence-deepens-the-window finding), not an independently new phenomenon.

**Where this lands instead**: folded into `THRESHOLD_PROOF.md` §11.5 as a short supporting remark
explaining WHY the leading-order approximation's error grows with `lambda` (a flatter near-optimum
takes a wider `cost_a` range to resolve into a value close to the true valley bottom), with a
light citation to the Skiba/DNSS-point and Whittle-indexability-boundary literature -- not written
up as an independent section or result. This right-sizing (confirming a real but modest effect,
correctly scoping its novelty against prior art, and declining to inflate it into a bigger
standalone contribution than the evidence supports) is itself consistent with this whole
investigation's standing practice of not overclaiming.
