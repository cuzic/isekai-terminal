"""Task #64 gate check (per both Codex's and Fable's advice: try to break the
refined conjecture cheaply before attempting any proof). #63 found that all
5 targeted-search seeds' deepest dips sit exactly ON a belief-simplex
boundary (beta1=1.0 or beta2=1.0) -- itself a notable pattern -- and the one
verified policy-level multi-crossing found lives exactly on that boundary
(beta2=1.0). This raises the refined question THRESHOLD_PROOF.md now poses:
can single-crossing be proven to hold AWAY FROM the belief-simplex boundary?

This script directly attacks that refined conjecture: it explicitly EXCLUDES
boundary rows/columns when locating the deepest dip (forcing an INTERIOR
dip to be found instead), then runs the same targeted Nelder-Mead attack
used in #63 to try to push that interior dip into a genuine multi-crossing.
If this also succeeds, the "away from the boundary" refinement doesn't
help either. If it fails across many seeds, that's a much better gate
signal for attempting an actual proof of the refined (boundary-excluded)
conjecture than #63's boundary-dominated search was.

Run with: uv run python policy_multicrossing_interior_search_demo.py
"""

from __future__ import annotations

import json

import numpy as np
from scipy.optimize import minimize

from dmr import beliefgrid2d, channels, switching_curves, warm_standby
from policy_multicrossing_targeted_search_demo import (
    PARAM_KEYS, params_to_vector, vector_to_params, solve_and_get_d,
)

INTERIOR_MARGIN = 0.05  # exclude the outer 5% of the belief square on each side


def find_deepest_interior_dip(sol, p: int, w: int):
    """Like `policy_multicrossing_targeted_search_demo.find_deepest_dip`, but
    excludes any dip whose location touches within INTERIOR_MARGIN of a
    simplex edge (beta1 or beta2 in {0,1})."""
    d_full = switching_curves.d_field_full_model(sol, p, w)
    d_grid = d_full.reshape(sol.grid.shape)
    axis = sol.grid.axis
    lo, hi = INTERIOR_MARGIN, 1.0 - INTERIOR_MARGIN
    interior_mask = (axis >= lo) & (axis <= hi)

    diffs_b2 = d_grid[:, 1:] - d_grid[:, :-1]  # step in beta2, fixed beta1 row i
    diffs_b1 = d_grid[1:, :] - d_grid[:-1, :]  # step in beta1, fixed beta2 column j

    candidates = []
    # beta2-direction: row i (beta1) must be interior, AND both endpoint columns interior
    valid_b2 = diffs_b2.copy()
    valid_b2[~interior_mask, :] = 0.0
    valid_b2[:, ~interior_mask[1:]] = 0.0
    valid_b2[:, ~interior_mask[:-1]] = 0.0
    if valid_b2.min() < 0:
        i, j = np.unravel_index(np.argmin(valid_b2), valid_b2.shape)
        candidates.append((float(axis[i]), float(axis[j + 1]), float(d_grid[i, j + 1]), float(-valid_b2[i, j])))

    valid_b1 = diffs_b1.copy()
    valid_b1[:, ~interior_mask] = 0.0
    valid_b1[~interior_mask[1:], :] = 0.0
    valid_b1[~interior_mask[:-1], :] = 0.0
    if valid_b1.min() < 0:
        i, j = np.unravel_index(np.argmin(valid_b1), valid_b1.shape)
        candidates.append((float(axis[i + 1]), float(axis[j]), float(d_grid[i + 1, j]), float(-valid_b1[i, j])))

    if not candidates:
        return None
    return max(candidates, key=lambda c: c[3])


def objective(vec: np.ndarray, beta1: float, beta2: float, p: int, w: int, opt_resolution: int) -> float:
    params = vector_to_params(vec)
    try:
        sol = solve_and_get_d(params, resolution=opt_resolution, n_iters=600)
    except Exception:
        return 10.0
    d_full = switching_curves.d_field_full_model(sol, p, w)
    d_val = float(sol.grid.interpolate_batch(d_full, np.array([beta1]), np.array([beta2]))[0])
    return abs(d_val)


def run_interior_search(seed_params: dict, label: str, locate_resolution: int = 30,
                         opt_resolution: int = 30) -> dict:
    print(f"\n=== Interior-only targeted search seeded from: {label} ===")
    sol_locate = solve_and_get_d(seed_params, resolution=locate_resolution, n_iters=1500)
    best_dip = {"depth": 0.0, "p": None, "w": None, "beta1": None, "beta2": None, "d": None}
    for p in range(2):
        for w in range(2):
            found = find_deepest_interior_dip(sol_locate, p, w)
            if found is None:
                continue
            beta1, beta2, d_val, depth = found
            if depth > best_dip["depth"]:
                best_dip = {"depth": depth, "p": p, "w": w, "beta1": beta1, "beta2": beta2, "d": d_val}

    if best_dip["p"] is None:
        print(f"  no INTERIOR dip found in this seed even at resolution={locate_resolution} -- skipping")
        return {"multi_crossing_found": False, "params": seed_params, "had_dip": False}

    print(f"  deepest interior dip: context=({best_dip['p']},{best_dip['w']}), "
          f"(beta1,beta2)=({best_dip['beta1']:.4f},{best_dip['beta2']:.4f}), "
          f"d={best_dip['d']:.4e}, depth={best_dip['depth']:.4e}")

    x0 = params_to_vector(seed_params)
    obj_args = (best_dip["beta1"], best_dip["beta2"], best_dip["p"], best_dip["w"], opt_resolution)
    # maxiter=600 (not 150) + logging result.success/nit/message, per Codex review (2026-07-18):
    # the original version couldn't distinguish "optimizer converged, this IS the best it can do"
    # from "optimizer was simply cut off before converging" -- both look identical without this.
    result = minimize(
        objective, x0, args=obj_args,
        method="Nelder-Mead",
        options={"maxiter": 600, "xatol": 1e-4, "fatol": 1e-6, "adaptive": True},
    )
    optimized_params = vector_to_params(result.x)
    print(f"  optimization: |d at interior dip| {objective(x0, *obj_args):.4e} -> {result.fun:.4e} "
          f"(converged={result.success}, nit={result.nit}, nfev={result.nfev}, msg={result.message!r})")

    sol_check = solve_and_get_d(optimized_params, resolution=60, n_iters=2000)
    total_multi = 0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol_check, p, w)
            curve1 = switching_curves.extract_level_curve(d_full, sol_check.grid, level=0.0)
            field_grid = d_full.reshape(sol_check.grid.shape)
            curve2 = switching_curves.extract_level_curve(field_grid.T.reshape(-1), sol_check.grid, level=0.0)
            total_multi += len(curve1.multi_crossing_columns) + len(curve2.multi_crossing_columns)
    print(f"  policy-level multi-crossings at optimized point (resolution=60): {total_multi}")
    return {"multi_crossing_found": total_multi > 0, "params": optimized_params, "had_dip": True}


def main() -> None:
    with open("output/adversarial_search_log.json") as f:
        log = json.load(f)
    violators = [t for t in log["trials"] if t.get("total_viol", 0) > 0]
    violators.sort(key=lambda t: -t["total_viol"])

    any_found = False
    n_had_dip = 0
    for t in violators:  # try ALL 12 known violators, not just top 5, since interior dips are rarer
        result = run_interior_search(t["params"], f"trial {t['trial']} (field_viol={t['total_viol']})")
        if result["had_dip"]:
            n_had_dip += 1
        if result["multi_crossing_found"]:
            any_found = True
            print(f"  *** INTERIOR POLICY-LEVEL MULTI-CROSSING FOUND *** params: {result['params']}")

    print(f"\n{n_had_dip}/{len(violators)} known violators have an interior (non-boundary) dip at all")
    print("\n=== Verdict ===")
    if any_found:
        print("An interior multi-crossing WAS found -- the boundary-exclusion refinement of the")
        print("single-crossing conjecture does NOT help; policy multi-crossings are not confined")
        print("to the simplex boundary. Task #64's proof attempt should not assume interior safety.")
    else:
        print("No interior multi-crossing found across all 12 known field-violators' interior dips")
        print("(where an interior dip exists at all). Combined with #63's single verified boundary")
        print("counterexample, this is a (still not proof-level) positive signal for the refined")
        print("conjecture 'single-crossing holds away from the belief-simplex boundary' -- worth")
        print("attempting as task #64's actual proof target, though this gate check alone cannot")
        print("rule out an interior counterexample existing elsewhere in the 12-dimensional space.")


if __name__ == "__main__":
    main()
