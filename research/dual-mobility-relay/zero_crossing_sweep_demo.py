"""D1-1 (task #44), extended: the single worst-case witness (trial 90) showed
the routing policy stays a clean single-threshold curve in both directions
despite `d_field_full_model` being non-monotone (see zero_crossing_check_demo.py).
Before treating that as the general answer, check it across ALL 12 trials
that showed a monotonicity violation in the seed=12345 adversarial search
(output/adversarial_search_log.json) -- not just the worst one -- at the
native resolution=30 used in the search itself (cheap; only escalate
resolution for any trial that actually shows a multi-crossing).

Run with: uv run python zero_crossing_sweep_demo.py
"""

from __future__ import annotations

import json

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

LOG_PATH = "output/adversarial_search_log.json"


def extract_level_curve_beta2_direction(field_flat, grid, level: float):
    field_grid = field_flat.reshape(grid.shape)
    return switching_curves.extract_level_curve(field_grid.T.reshape(-1), grid, level)


def check_trial(params: dict, resolution: int, n_iters: int = 800):
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)
    multi1 = multi2 = 0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            curve1 = switching_curves.extract_level_curve(d_full, sol.grid, level=0.0)
            curve2 = extract_level_curve_beta2_direction(d_full, sol.grid, level=0.0)
            multi1 += len(curve1.multi_crossing_columns)
            multi2 += len(curve2.multi_crossing_columns)
    return multi1, multi2


def main() -> None:
    with open(LOG_PATH) as f:
        log = json.load(f)
    violators = [t for t in log["trials"] if t.get("total_viol", 0) > 0]
    print(f"=== Checking all {len(violators)} monotonicity-violating trials for policy multi-crossing ===")

    any_policy_break = False
    for t in violators:
        multi1, multi2 = check_trial(t["params"], resolution=30)
        flag = " <-- POLICY THRESHOLD BROKEN" if (multi1 or multi2) else ""
        print(f"trial {t['trial']:>3}: field_viol={t['total_viol']:>4}, "
              f"policy_multi_crossing(beta1-dir)={multi1}, (beta2-dir)={multi2}{flag}")
        if multi1 or multi2:
            any_policy_break = True

    print("\n=== Verdict ===")
    if any_policy_break:
        print("At least one violating trial shows the routing POLICY itself losing its threshold")
        print("structure (not just the underlying d-field) -- Gap G1 is policy-relevant there.")
    else:
        print(f"None of the {len(violators)} field-monotonicity-violating trials show a policy")
        print("multi-crossing at resolution=30. Combined with the resolution-150 recheck of the")
        print("worst trial (zero_crossing_check_demo.py), this is consistent evidence that Gap G1's")
        print("non-monotonicity, across this random sweep, stays a FIELD-level phenomenon: the")
        print("routing policy remains a clean single threshold in both beta1 and beta2 even where")
        print("d_field_full_model itself is not monotone. This does not prove it always holds (no")
        print("theorem), but it is the relevant empirical answer for #42/#49/#53/#58.")


if __name__ == "__main__":
    main()
