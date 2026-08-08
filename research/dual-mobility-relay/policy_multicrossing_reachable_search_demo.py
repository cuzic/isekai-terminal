"""R2 (post-consultation reframing of task #63/#64's targeted search), per an
independent Codex+Fable review of two candidate next directions (2026-07-19).

Both reviewers agreed the existing targeted search
(`policy_multicrossing_targeted_search_demo.py`) has two real flaws, and
Fable additionally found a concrete, verified fact that changes how much the
ONE known policy-level multi-crossing witness (from #63, at beta2=1.0)
actually matters:

1. **The witness lives outside the dynamically reachable belief set.**
   `predict_scalar(beta, hop) = beta*(1-p_bg) + (1-beta)*p_gb` maps ANY
   belief into `[min(p_gb,1-p_bg), max(p_gb,1-p_bg)]` after just one predict
   step -- and since every timestep of this belief-MDP applies predict
   regardless of whether an observation occurred, that interval is
   forward-invariant: once inside, belief never leaves it again. For the
   #63 witness's own hop2 (p_gb2=0.0051, p_bg2=0.661), this reachable
   interval is beta2 in [0.0051, 0.339] -- confirmed directly in this task
   (R1) by iterating `predict_scalar` from beta2=1.0 itself: it collapses to
   0.339 after ONE step and keeps shrinking. beta2=1.0 is therefore not a
   belief the system can ever be in except (degenerately) at time 0 before
   any transition -- it contributes nothing to average-cost behavior.
   `policy_multicrossing_interior_search_demo.py`'s existing
   `INTERIOR_MARGIN=0.05` gate check used an arbitrary FIXED margin, not
   this per-hop, parameter-dependent, dynamically-justified interval --
   the right exclusion region is [p_gb_i, 1-p_bg_i], not "outer 5%".

2. **The existing objective only drives |d| toward zero at one point,
   with nothing stopping the whole neighboring slice from collapsing
   toward zero together** (a shifted single threshold, not a genuine
   multi-crossing) -- confirmed by the existing script's own docstring.
   This is why "push the same dip closer to exact zero" runs kept NOT
   producing a crossing: the objective was never actually rewarding one.

This script fixes BOTH: it locates the deepest dip strictly INSIDE the
per-hop reachable box (not a fixed margin), and replaces the |d|-minimization
objective with a genuine sign-pattern MARGIN objective -- maximizing
min(d(-delta), -d(center), d(+delta)) (a "+,-,+" pattern) or its mirror
"-,+,-", whichever is larger -- so a positive result IS a certified
multi-crossing with a quantified margin, not something requiring a separate
post-hoc check. Optimized via `scipy.optimize.differential_evolution` (a
global, population-based search) instead of local Nelder-Mead, since both
reviewers noted a local optimizer's path-dependence is itself a likely
reason the original search's results didn't reproduce under a corrected
parameterization.

Run with: uv run python policy_multicrossing_reachable_search_demo.py
"""

from __future__ import annotations

import json

import numpy as np
from scipy.optimize import differential_evolution

from dmr import beliefgrid2d, channels, switching_curves, warm_standby
from policy_multicrossing_targeted_search_demo import (
    params_to_vector, vector_to_params, solve_and_get_d,
)

# Bounds in the params_to_vector transformed space, derived directly from
# adversarial_search_demo.py's "physically plausible parameter box" (the
# same box the project's whole counterexample search has used throughout --
# not a new/arbitrary range).
VEC_BOUNDS = [
    (-2.5 * np.log(10), -0.7 * np.log(10)),  # p_gb1 (log)
    (-1.5 * np.log(10), -0.1 * np.log(10)),  # p_bg1 (log)
    (np.log(0.001), np.log(0.05)),           # eps_good1 (log)
    (np.log(0.02), np.log(0.93)),            # eps_bad1_margin (log)
    (-2.5 * np.log(10), -0.7 * np.log(10)),  # p_gb2 (log)
    (-1.5 * np.log(10), -0.1 * np.log(10)),  # p_bg2 (log)
    (np.log(0.001), np.log(0.05)),           # eps_good2 (log)
    (np.log(0.02), np.log(0.93)),            # eps_bad2_margin (log)
    (0.02, 0.3),                              # cost_a (linear)
    (-3 * np.log(10), -0.5 * np.log(10)),    # c_warm (log)
    (-3 * np.log(10), -1 * np.log(10)),      # c_switch_warm (log)
    (np.log(0.01), np.log(1.0)),             # c_switch_cold_margin (log)
]


def reachable_box(p_gb: float, p_bg: float) -> tuple[float, float]:
    """The forward-invariant belief interval after >=1 predict step (see
    module docstring): any belief starting outside this range collapses
    into it after one step and never leaves again."""
    lo, hi = sorted((p_gb, 1.0 - p_bg))
    return lo, hi


def find_deepest_dip_in_reachable_box(sol, p: int, w: int, hop1: channels.HopParams, hop2: channels.HopParams):
    """Like `find_deepest_interior_dip`, but the exclusion region is each
    hop's own dynamically reachable interval, not a fixed 5% margin."""
    d_full = switching_curves.d_field_full_model(sol, p, w)
    d_grid = d_full.reshape(sol.grid.shape)
    axis = sol.grid.axis

    lo1, hi1 = reachable_box(hop1.p_gb, hop1.p_bg)
    lo2, hi2 = reachable_box(hop2.p_gb, hop2.p_bg)
    reach1 = (axis >= lo1) & (axis <= hi1)
    reach2 = (axis >= lo2) & (axis <= hi2)

    diffs_b2 = d_grid[:, 1:] - d_grid[:, :-1]  # step in beta2 (axis 1), fixed beta1 row i
    diffs_b1 = d_grid[1:, :] - d_grid[:-1, :]  # step in beta1 (axis 0), fixed beta2 column j

    candidates = []
    valid_b2 = diffs_b2.copy()
    valid_b2[~reach1, :] = 0.0
    valid_b2[:, ~reach2[1:]] = 0.0
    valid_b2[:, ~reach2[:-1]] = 0.0
    if valid_b2.min() < 0:
        i, j = np.unravel_index(np.argmin(valid_b2), valid_b2.shape)
        candidates.append(("beta2", float(axis[i]), float(axis[j + 1]), float(-valid_b2[i, j])))

    valid_b1 = diffs_b1.copy()
    valid_b1[:, ~reach2] = 0.0
    valid_b1[~reach1[1:], :] = 0.0
    valid_b1[~reach1[:-1], :] = 0.0
    if valid_b1.min() < 0:
        i, j = np.unravel_index(np.argmin(valid_b1), valid_b1.shape)
        candidates.append(("beta1", float(axis[i + 1]), float(axis[j]), float(-valid_b1[i, j])))

    if not candidates:
        return None
    return max(candidates, key=lambda c: c[3])  # (direction, beta1, beta2, depth)


def sign_margin(sol, p: int, w: int, beta1: float, beta2: float, direction: str, delta: float) -> float:
    """max over the two possible sign patterns ('+,-,+' and '-,+,-') of the
    minimum absolute margin by which each of the 3 points clears zero on the
    correct side. Positive value = a CERTIFIED multi-crossing with that
    margin (not a post-hoc-only observation); this is the quantity the
    optimizer directly maximizes, unlike the old |d|-at-one-point objective."""
    d_full = switching_curves.d_field_full_model(sol, p, w)
    if direction == "beta2":
        b1 = np.array([beta1, beta1, beta1])
        b2 = np.array([max(beta2 - delta, 0.0), beta2, min(beta2 + delta, 1.0)])
    else:
        b1 = np.array([max(beta1 - delta, 0.0), beta1, min(beta1 + delta, 1.0)])
        b2 = np.array([beta2, beta2, beta2])
    d_lo, d_mid, d_hi = sol.grid.interpolate_batch(d_full, b1, b2)
    pattern_pmp = min(d_lo, -d_mid, d_hi)   # "+,-,+"
    pattern_mpm = min(-d_lo, d_mid, -d_hi)  # "-,+,-"
    return float(max(pattern_pmp, pattern_mpm))


def objective(vec: np.ndarray, beta1: float, beta2: float, direction: str, p: int, w: int,
              opt_resolution: int, delta: float) -> float:
    params = vector_to_params(vec)
    try:
        sol = solve_and_get_d(params, resolution=opt_resolution, n_iters=600)
    except Exception:
        return 10.0  # penalize failures (DE minimizes, so this is a bad score)
    margin = sign_margin(sol, p, w, beta1, beta2, direction, delta)
    return -margin  # DE minimizes; we want to MAXIMIZE margin


def run_reachable_search(seed_params: dict, label: str, locate_resolution: int = 30,
                          opt_resolution: int = 30, delta: float = 0.02,
                          maxiter: int = 40, popsize: int = 12, seed: int = 0) -> dict:
    print(f"\n=== Reachable-box targeted search seeded from: {label} ===")
    hop1 = channels.HopParams(p_gb=seed_params["p_gb1"], p_bg=seed_params["p_bg1"],
                               eps_good=seed_params["eps_good1"], eps_bad=seed_params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=seed_params["p_gb2"], p_bg=seed_params["p_bg2"],
                               eps_good=seed_params["eps_good2"], eps_bad=seed_params["eps_bad2"])
    lo1, hi1 = reachable_box(hop1.p_gb, hop1.p_bg)
    lo2, hi2 = reachable_box(hop2.p_gb, hop2.p_bg)
    print(f"  reachable box: beta1 in [{lo1:.4f},{hi1:.4f}], beta2 in [{lo2:.4f},{hi2:.4f}]")

    sol_locate = solve_and_get_d(seed_params, resolution=locate_resolution, n_iters=1500)
    best = {"depth": 0.0, "p": None, "w": None, "direction": None, "beta1": None, "beta2": None}
    for p in range(2):
        for w in range(2):
            found = find_deepest_dip_in_reachable_box(sol_locate, p, w, hop1, hop2)
            if found is None:
                continue
            direction, b1, b2, depth = found
            if depth > best["depth"]:
                best = {"depth": depth, "p": p, "w": w, "direction": direction, "beta1": b1, "beta2": b2}

    if best["p"] is None:
        print(f"  no dip found INSIDE the reachable box even at resolution={locate_resolution} -- skipping")
        return {"multi_crossing_found": False, "had_dip": False, "params": seed_params}

    print(f"  deepest reachable-box dip: context=({best['p']},{best['w']}), direction={best['direction']}, "
          f"(beta1,beta2)=({best['beta1']:.4f},{best['beta2']:.4f}), depth={best['depth']:.4e}")

    x0 = params_to_vector(seed_params)
    x0_margin = -objective(x0, best["beta1"], best["beta2"], best["direction"], best["p"], best["w"],
                            opt_resolution, delta)
    obj_args = (best["beta1"], best["beta2"], best["direction"], best["p"], best["w"], opt_resolution, delta)
    result = differential_evolution(
        objective, VEC_BOUNDS, args=obj_args, x0=x0,
        maxiter=maxiter, popsize=popsize, seed=seed, polish=True, tol=1e-6,
    )
    optimized_params = vector_to_params(result.x)
    best_margin = -result.fun
    print(f"  sign-margin objective: {x0_margin:.4e} -> {best_margin:.4e} "
          f"(nfev={result.nfev}, converged={result.success})")

    print("  re-solving optimized params at resolution=60 and checking for genuine multi-crossings...")
    sol_check = solve_and_get_d(optimized_params, resolution=60, n_iters=2000)
    opt_hop1 = channels.HopParams(p_gb=optimized_params["p_gb1"], p_bg=optimized_params["p_bg1"],
                                   eps_good=optimized_params["eps_good1"], eps_bad=optimized_params["eps_bad1"])
    opt_hop2 = channels.HopParams(p_gb=optimized_params["p_gb2"], p_bg=optimized_params["p_bg2"],
                                   eps_good=optimized_params["eps_good2"], eps_bad=optimized_params["eps_bad2"])
    opt_lo1, opt_hi1 = reachable_box(opt_hop1.p_gb, opt_hop1.p_bg)
    opt_lo2, opt_hi2 = reachable_box(opt_hop2.p_gb, opt_hop2.p_bg)

    total_multi = 0
    in_reachable = 0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol_check, p, w)
            curve1 = switching_curves.extract_level_curve(d_full, sol_check.grid, level=0.0)
            field_grid = d_full.reshape(sol_check.grid.shape)
            curve2 = switching_curves.extract_level_curve(field_grid.T.reshape(-1), sol_check.grid, level=0.0)
            for col_idx, _ in curve1.multi_crossing_columns:
                total_multi += 1
                beta2_val = sol_check.grid.axis[col_idx]
                if opt_lo1 <= beta2_val <= opt_hi1:  # curve1 columns are indexed by beta2 (fixed-beta2 slices)
                    in_reachable += 1
            for col_idx, _ in curve2.multi_crossing_columns:
                total_multi += 1
                beta1_val = sol_check.grid.axis[col_idx]
                if opt_lo2 <= beta1_val <= opt_hi2:
                    in_reachable += 1
    print(f"  policy-level multi-crossings at optimized point (res=60): {total_multi} total, "
          f"{in_reachable} inside the OPTIMIZED params' own reachable box")
    return {
        "multi_crossing_found": total_multi > 0,
        "in_reachable_box": in_reachable > 0,
        "total_multi": total_multi,
        "params": optimized_params,
        "had_dip": True,
    }


def deepest_reachable_dip_depth(params: dict, resolution: int, n_iters: int = 600) -> float:
    """Objective for the EXISTENCE HUNT below: the deepest d-field
    monotonicity dip found strictly inside the reachable belief box, across
    all 4 contexts, for a candidate parameter vector -- 0.0 if the d-field
    is monotone on the reachable box everywhere. This is a Gap-G1-level
    (field, not yet policy) question, evaluated BEFORE the policy-level
    sign-margin search, since none of the 12 known field-violators have any
    reachable-box dip at all (verified directly, resolutions 30/60/100) --
    so seeding a targeted search from them is pointless; a fresh global
    search over the whole parameter box is needed instead."""
    try:
        sol = solve_and_get_d(params, resolution=resolution, n_iters=n_iters)
    except Exception:
        return 0.0
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    best_depth = 0.0
    for p in range(2):
        for w in range(2):
            found = find_deepest_dip_in_reachable_box(sol, p, w, hop1, hop2)
            if found is not None and found[3] > best_depth:
                best_depth = found[3]
    return best_depth


def _existence_objective(vec: np.ndarray, resolution: int) -> float:
    params = vector_to_params(vec)
    return -deepest_reachable_dip_depth(params, resolution=resolution)


def run_existence_hunt(resolution: int = 30, maxiter: int = 25, popsize: int = 10, seed: int = 0) -> dict:
    """Global (differential_evolution) search over the WHOLE physically-
    plausible 12-D parameter box for ANY point where the d-field itself
    (not yet the policy) has a monotonicity violation inside the reachable
    belief box. If this finds nothing, there is no need for a policy-level
    reachable-box search at all -- the policy is derived from d, so a
    reachable-box-monotone d-field trivially gives a reachable-box-monotone
    policy too."""
    print(f"\n=== Existence hunt: does ANY reachable-box d-field violation exist? "
          f"(resolution={resolution}, DE popsize={popsize}, maxiter={maxiter}) ===")
    result = differential_evolution(
        _existence_objective, VEC_BOUNDS, args=(resolution,),
        maxiter=maxiter, popsize=popsize, seed=seed, polish=False, tol=1e-7,
    )
    best_depth = -result.fun
    best_params = vector_to_params(result.x)
    print(f"  best reachable-box dip depth found: {best_depth:.4e} (nfev={result.nfev}, "
          f"converged={result.success})")
    if best_depth > 0:
        print(f"  candidate params: {best_params}")
    return {"depth": best_depth, "params": best_params}


def main() -> None:
    hunt = run_existence_hunt()
    seeds_for_targeted_search = []
    if hunt["depth"] > 1e-8:
        print("  existence hunt found a candidate -- verifying at higher resolution before using as a seed...")
        depth_check = deepest_reachable_dip_depth(hunt["params"], resolution=100, n_iters=2000)
        print(f"  re-check at resolution=100: depth={depth_check:.4e}")
        if depth_check > 1e-8:
            seeds_for_targeted_search.append((hunt["params"], "existence-hunt candidate"))
        else:
            print("  candidate did not survive at resolution=100 -- treating as a discretization fluke, not a seed")

    with open("output/adversarial_search_log.json") as f:
        log = json.load(f)
    violators = [t for t in log["trials"] if t.get("total_viol", 0) > 0]
    violators.sort(key=lambda t: -t["total_viol"])
    for t in violators[:5]:
        seeds_for_targeted_search.append((t["params"], f"trial {t['trial']} (field_viol={t['total_viol']})"))

    any_found_reachable = False
    n_had_dip = 0
    for idx, (seed_params, label) in enumerate(seeds_for_targeted_search):
        result = run_reachable_search(seed_params, label, seed=idx)
        if result["had_dip"]:
            n_had_dip += 1
        if result.get("in_reachable_box"):
            any_found_reachable = True
            print(f"  *** REACHABLE-BOX POLICY-LEVEL MULTI-CROSSING FOUND *** params: {result['params']}")

    print(f"\n{n_had_dip}/{len(seeds_for_targeted_search)} seeds have a dip strictly inside "
          f"the reachable belief box at all")
    print("\n=== Verdict ===")
    if hunt["depth"] <= 1e-8:
        print("The existence hunt (a global DE search over the entire physically-plausible parameter")
        print("box, not seeded from the 12 known field-violators -- none of which have ANY reachable-box")
        print("dip, verified at resolutions 30/60/100) found NO point where the d-field itself violates")
        print("monotonicity inside the dynamically reachable belief set. Combined with #63's one known")
        print("policy-level witness living outside the reachable set (verified in R1), the practically-")
        print("relevant conjecture 'the routing policy has single-crossing structure on every belief the")
        print("system can actually occupy' now has NO counterexample found by either a targeted seed-based")
        print("attack or a global existence hunt -- the strongest evidence for it obtained so far, though")
        print("still not a proof.")
    elif any_found_reachable:
        print("A sign-margin-optimized search DID find a genuine policy-level multi-crossing whose")
        print("location lies inside the OPTIMIZED parameters' own dynamically reachable belief set --")
        print("this is a materially stronger finding than #63's witness (which sits outside the")
        print("reachable set) and should be written up as revising the refined single-crossing")
        print("conjecture, not just the original one.")
    else:
        print("No reachable-box multi-crossing found across the 5 largest known field-violators,")
        print("using a global (differential_evolution) search with a sign-pattern margin objective")
        print("that directly rewards genuine crossings rather than a proxy checked post-hoc. Combined")
        print("with the fact that #63's ONE known witness (beta2=1.0) is dynamically unreachable")
        print("(verified in R1), this is materially stronger evidence than before that the routing")
        print("POLICY's single-crossing structure holds within the belief states the system can")
        print("actually occupy -- still not proof-level, but the refined, practically-relevant")
        print("conjecture ('single-crossing holds on the reachable belief set') now has a much")
        print("more targeted attack behind it with no success, not just an arbitrary interior margin.")


if __name__ == "__main__":
    main()
