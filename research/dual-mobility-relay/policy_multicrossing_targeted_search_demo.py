"""Task #63 (A0-2): a TARGETED search for a genuine policy-level multi-
crossing, not another blind random sweep (already done in #44/#45, which
found zero policy-level breaks among the 12+15 known field-level
violators). Per Codex's 2026-07-18 review, this conditions the search on
"the non-monotone dip is close to zero" rather than sampling uniformly and
hoping to stumble on a near-tangent case.

Method: for a known field-violation witness, locate the (beta1, beta2)
coordinate of that witness's deepest non-monotone dip (checking BOTH the
beta1- and beta2-direction diffs -- an earlier version of this script only
checked beta2, silently missing 4 of 5 seeds whose violations happen to be
beta1-direction, caught and fixed during this task's own run). Then use
gradient-free local optimization (`scipy.optimize.minimize`, Nelder-Mead --
the objective comes from an iterative RVI solve, so no gradient is
available) to search the 12-dimensional parameter space for a nearby point
where `d_field_full_model`'s value AT THAT FIXED belief coordinate is driven
toward zero. IMPORTANT (per Codex review, 2026-07-18): the objective ONLY
minimizes `|d|` at that one fixed point -- it does NOT penalize or otherwise
constrain the neighboring points on the same slice, so nothing during
optimization stops the whole neighborhood from collapsing toward zero
together (which would NOT produce a multi-crossing, just a shifted single
threshold). Whether a genuine multi-crossing resulted is therefore purely a
POST-HOC question, answered only by directly re-solving the optimized point
and checking `extract_level_curve`'s `multi_crossing_columns` afterward --
not something the optimization itself was constructed to guarantee.

After optimization, this directly checks `switching_curves.extract_level_curve`'s
`multi_crossing_columns` on the optimized point to see whether an actual
policy-level break was found -- the optimization objective is only a proxy
for this, and it succeeding on the objective does not automatically mean
the policy actually broke (the local slice's OTHER points also need to stay
outside the danger zone for it to be a genuine multi-crossing, not just a
near-zero touch).

Run with: uv run python policy_multicrossing_targeted_search_demo.py
"""

from __future__ import annotations

import json

import numpy as np
from scipy.optimize import minimize

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

PARAM_KEYS = ["p_gb1", "p_bg1", "eps_good1", "eps_bad1", "p_gb2", "p_bg2", "eps_good2", "eps_bad2",
              "cost_a", "c_warm", "c_switch_warm", "c_switch_cold"]


def solve_and_get_d(params: dict, resolution: int, n_iters: int = 1000):
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)
    return sol


def find_deepest_dip(sol, p: int, w: int) -> tuple[float, float, float, float]:
    """Returns (beta1, beta2, d_value, depth) of the largest single-step
    drop in d along EITHER axis (beta1-direction or beta2-direction --
    checking both, matching `check_monotone_grid`'s definition of a
    violation, not just the beta2 direction the trial-90 witness happened
    to concentrate its violations in), in CONTINUOUS belief coordinates.

    FIXED TWICE per bugs caught during this task's own run: (1) the
    original version returned grid indices reused directly as indices into
    a different-resolution solve's grid, only valid if resolutions match --
    fixed by returning continuous (beta1,beta2) coordinates instead. (2) the
    original version only checked the beta2-direction diff, silently
    missing any seed whose violations are concentrated in the beta1
    direction instead -- 4 of 5 seeds (trial 64/126/23/24) reported "no dip
    found" even at resolution=30 with only the beta2-direction check, which
    is suspicious given they are confirmed violators of `check_monotone_grid`
    (which checks both axes) at that same resolution in the original search
    log. Checking both axes and taking whichever has the deeper dip fixes
    this."""
    d_full = switching_curves.d_field_full_model(sol, p, w)
    d_grid = d_full.reshape(sol.grid.shape)
    axis = sol.grid.axis

    diffs_b2 = d_grid[:, 1:] - d_grid[:, :-1]  # step in beta2, fixed beta1 row
    diffs_b1 = d_grid[1:, :] - d_grid[:-1, :]  # step in beta1, fixed beta2 column

    candidates = []
    if diffs_b2.min() < 0:
        i, j = np.unravel_index(np.argmin(diffs_b2), diffs_b2.shape)
        candidates.append((float(axis[i]), float(axis[j + 1]), float(d_grid[i, j + 1]), float(-diffs_b2.min())))
    if diffs_b1.min() < 0:
        i, j = np.unravel_index(np.argmin(diffs_b1), diffs_b1.shape)
        candidates.append((float(axis[i + 1]), float(axis[j]), float(d_grid[i + 1, j]), float(-diffs_b1.min())))

    if not candidates:
        return None
    return max(candidates, key=lambda c: c[3])


def params_to_vector(params: dict) -> np.ndarray:
    """Log-parameterizes positive-scale quantities to keep the optimizer in a
    sane space. FIXED per Codex review (2026-07-18): the original version
    optimized eps_bad and c_switch_cold directly, with only a post-hoc clip
    to [1e-4,0.999]/[1e-5,5.0] -- nothing stopped the optimizer from
    wandering into `eps_bad < eps_good` ("Bad state loses less than Good") or
    `c_switch_cold < c_switch_warm` ("switching cold is cheaper than
    switching warm"), both physically backwards. The reported counterexample
    happens not to be contaminated (its eps_bad>eps_good and
    c_switch_cold>>c_switch_warm, nowhere near either clip boundary), but the
    optimizer could produce a contaminated point on a future run. Fixed by
    reparameterizing eps_bad/c_switch_cold as (a positive margin above
    eps_good/c_switch_warm), log-parameterized so the margin can't go
    negative."""
    v = dict(params)
    v["eps_bad1_margin"] = np.log(max(params["eps_bad1"] - params["eps_good1"], 1e-6))
    v["eps_bad2_margin"] = np.log(max(params["eps_bad2"] - params["eps_good2"], 1e-6))
    v["c_switch_cold_margin"] = np.log(max(params["c_switch_cold"] - params["c_switch_warm"], 1e-6))
    log_keys = ["p_gb1", "p_bg1", "eps_good1", "p_gb2", "p_bg2", "eps_good2", "c_warm", "c_switch_warm"]
    order = ["p_gb1", "p_bg1", "eps_good1", "eps_bad1_margin", "p_gb2", "p_bg2", "eps_good2",
             "eps_bad2_margin", "cost_a", "c_warm", "c_switch_warm", "c_switch_cold_margin"]
    return np.array([np.log(v[k]) if k in log_keys else v[k] for k in order])


def vector_to_params(vec: np.ndarray) -> dict:
    log_keys = {"p_gb1", "p_bg1", "eps_good1", "p_gb2", "p_bg2", "eps_good2", "c_warm", "c_switch_warm"}
    order = ["p_gb1", "p_bg1", "eps_good1", "eps_bad1_margin", "p_gb2", "p_bg2", "eps_good2",
             "eps_bad2_margin", "cost_a", "c_warm", "c_switch_warm", "c_switch_cold_margin"]
    v = {}
    for k, x in zip(order, vec):
        v[k] = float(np.exp(x)) if k in log_keys else float(x)

    for k in ["p_gb1", "p_bg1", "eps_good1", "p_gb2", "p_bg2", "eps_good2"]:
        v[k] = float(np.clip(v[k], 1e-4, 0.98))
    v["cost_a"] = float(np.clip(v["cost_a"], 1e-4, 2.0))
    for k in ["c_warm", "c_switch_warm"]:
        v[k] = float(np.clip(v[k], 1e-5, 5.0))

    eps_bad1_margin = float(np.clip(np.exp(vec[3]), 1e-4, 0.98 - v["eps_good1"]))
    eps_bad2_margin = float(np.clip(np.exp(vec[7]), 1e-4, 0.98 - v["eps_good2"]))
    c_switch_cold_margin = float(np.clip(np.exp(vec[11]), 1e-5, 5.0 - v["c_switch_warm"]))

    return {
        "p_gb1": v["p_gb1"], "p_bg1": v["p_bg1"],
        "eps_good1": v["eps_good1"], "eps_bad1": v["eps_good1"] + eps_bad1_margin,
        "p_gb2": v["p_gb2"], "p_bg2": v["p_bg2"],
        "eps_good2": v["eps_good2"], "eps_bad2": v["eps_good2"] + eps_bad2_margin,
        "cost_a": v["cost_a"], "c_warm": v["c_warm"], "c_switch_warm": v["c_switch_warm"],
        "c_switch_cold": v["c_switch_warm"] + c_switch_cold_margin,
    }


def objective(vec: np.ndarray, beta1: float, beta2: float, p: int, w: int, opt_resolution: int) -> float:
    params = vector_to_params(vec)
    try:
        sol = solve_and_get_d(params, resolution=opt_resolution, n_iters=600)
    except Exception:
        return 10.0  # penalize failures
    d_full = switching_curves.d_field_full_model(sol, p, w)
    d_val = float(sol.grid.interpolate_batch(d_full, np.array([beta1]), np.array([beta2]))[0])
    return abs(d_val)


def run_targeted_search(seed_params: dict, label: str, locate_resolution: int = 30,
                         opt_resolution: int = 30) -> dict:
    # opt_resolution fixed to 30 per Codex review (2026-07-18) -- an earlier version left
    # this at 24 (a leftover from before the resolution-mismatch bug fix above), which risked
    # optimizing against a different-resolution field than the one the dip was located at.
    print(f"\n=== Targeted search seeded from: {label} ===")
    # Locate the dip at the SAME resolution these violations are known to exist at (30),
    # in continuous belief coordinates -- not the (cheaper) resolution used for optimization.
    sol_locate = solve_and_get_d(seed_params, resolution=locate_resolution, n_iters=1500)
    best_dip = {"depth": 0.0, "p": None, "w": None, "beta1": None, "beta2": None, "d": None}
    for p in range(2):
        for w in range(2):
            found = find_deepest_dip(sol_locate, p, w)
            if found is None:
                continue
            beta1, beta2, d_val, depth = found
            if depth > best_dip["depth"]:
                best_dip = {"depth": depth, "p": p, "w": w, "beta1": beta1, "beta2": beta2, "d": d_val}

    if best_dip["p"] is None:
        print(f"  no dip found in this seed scenario even at resolution={locate_resolution} -- skipping")
        return {"multi_crossing_found": False, "params": seed_params}

    print(f"  deepest dip: context=({best_dip['p']},{best_dip['w']}), "
          f"(beta1,beta2)=({best_dip['beta1']:.4f},{best_dip['beta2']:.4f}), "
          f"d={best_dip['d']:.4e}, depth={best_dip['depth']:.4e}")

    x0 = params_to_vector(seed_params)
    obj_args = (best_dip["beta1"], best_dip["beta2"], best_dip["p"], best_dip["w"], opt_resolution)
    result = minimize(
        objective, x0, args=obj_args,
        method="Nelder-Mead",
        options={"maxiter": 150, "xatol": 1e-3, "fatol": 1e-5, "adaptive": True},
    )
    optimized_params = vector_to_params(result.x)
    print(f"  optimization: |d at dip| {objective(x0, *obj_args):.4e} "
          f"-> {result.fun:.4e} ({result.nit} iterations)")

    print("  re-solving optimized params at resolution=60 and checking for genuine multi-crossings...")
    sol_check = solve_and_get_d(optimized_params, resolution=60, n_iters=2000)
    any_multi = False
    total_multi = 0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol_check, p, w)
            curve1 = switching_curves.extract_level_curve(d_full, sol_check.grid, level=0.0)
            field_grid = d_full.reshape(sol_check.grid.shape)
            curve2 = switching_curves.extract_level_curve(field_grid.T.reshape(-1), sol_check.grid, level=0.0)
            n_multi = len(curve1.multi_crossing_columns) + len(curve2.multi_crossing_columns)
            total_multi += n_multi
            if n_multi > 0:
                any_multi = True
    print(f"  policy-level multi-crossings at optimized point (resolution=60): {total_multi}")
    return {"multi_crossing_found": any_multi, "total_multi": total_multi, "params": optimized_params}


def main() -> None:
    with open("output/adversarial_search_log.json") as f:
        log = json.load(f)
    violators = [t for t in log["trials"] if t.get("total_viol", 0) > 0]
    violators.sort(key=lambda t: -t["total_viol"])

    any_found = False
    for t in violators[:5]:  # the 5 largest known field-violators as seeds
        result = run_targeted_search(t["params"], f"trial {t['trial']} (field_viol={t['total_viol']})")
        if result["multi_crossing_found"]:
            any_found = True
            print(f"  *** POLICY-LEVEL MULTI-CROSSING FOUND *** params: {result['params']}")

    print("\n=== Verdict ===")
    if any_found:
        print("A targeted search DID find a genuine policy-level multi-crossing -- the routing")
        print("policy CAN lose its threshold structure, not just the underlying d-field. This")
        print("changes #42/#49's conclusions and should be written up as a stronger finding.")
    else:
        print("The targeted search (Nelder-Mead minimizing |d| at each seed's deepest-dip location,")
        print("re-verified with extract_level_curve at resolution=60) found NO genuine policy-level")
        print("multi-crossing across the 5 largest known field-violators. Combined with #44/#45's")
        print("broader random sweep (0/12+15 witnesses) and this directed attack specifically trying")
        print("to construct one, this is stronger (though still not proof-level) evidence that the")
        print("routing policy's threshold structure is more robust than the underlying d-field's")
        print("monotonicity -- consistent with attempting task #64's single-crossing proof.")


if __name__ == "__main__":
    main()
