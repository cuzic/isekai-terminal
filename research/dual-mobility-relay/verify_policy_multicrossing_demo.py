"""Follow-up to task #63: `policy_multicrossing_targeted_search_demo.py`'s
targeted search found ONE candidate genuine policy-level multi-crossing
(seeded from trial 24, after Nelder-Mead optimization). Before treating this
as a real finding (it would revise #42/#49's conclusion that no witness
ever showed the routing policy itself losing its threshold structure), this
script independently verifies it: resolution-convergence check (is the
multi-crossing resolution-stable, or does it vanish/appear only at
resolution=60 as a fluke?), and exact identification of which context and
slice shows it.

Run with: uv run python verify_policy_multicrossing_demo.py
"""

from __future__ import annotations

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

CANDIDATE_PARAMS = {
    'p_gb1': 0.004020990797610466, 'p_bg1': 0.05219459934851931,
    'eps_good1': 0.03723314131734392, 'eps_bad1': 0.28972224762056686,
    'p_gb2': 0.0050988486422139994, 'p_bg2': 0.660923303621735,
    'eps_good2': 0.034764642885318026, 'eps_bad2': 0.8842840254396973,
    'cost_a': 0.11530432458734607, 'c_warm': 0.0027246169114145875,
    'c_switch_warm': 0.001616690587499065, 'c_switch_cold': 0.7762601796991225,
}


def check(resolution: int, n_iters: int = 2000):
    hop1 = channels.HopParams(p_gb=CANDIDATE_PARAMS["p_gb1"], p_bg=CANDIDATE_PARAMS["p_bg1"],
                               eps_good=CANDIDATE_PARAMS["eps_good1"], eps_bad=CANDIDATE_PARAMS["eps_bad1"])
    hop2 = channels.HopParams(p_gb=CANDIDATE_PARAMS["p_gb2"], p_bg=CANDIDATE_PARAMS["p_bg2"],
                               eps_good=CANDIDATE_PARAMS["eps_good2"], eps_bad=CANDIDATE_PARAMS["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, CANDIDATE_PARAMS["cost_a"], CANDIDATE_PARAMS["c_warm"],
        CANDIDATE_PARAMS["c_switch_warm"], CANDIDATE_PARAMS["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)

    print(f"\n--- resolution={resolution} ---")
    total_multi = 0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            curve1 = switching_curves.extract_level_curve(d_full, sol.grid, level=0.0)
            field_grid = d_full.reshape(sol.grid.shape)
            curve2 = switching_curves.extract_level_curve(field_grid.T.reshape(-1), sol.grid, level=0.0)
            n1, n2 = len(curve1.multi_crossing_columns), len(curve2.multi_crossing_columns)
            total_multi += n1 + n2
            if n1 or n2:
                print(f"  context (p={p}, w={w}): "
                      f"[fixed-beta2, root over beta1] multi-crossing columns: {curve1.multi_crossing_columns}")
                print(f"                            "
                      f"[fixed-beta1, root over beta2] multi-crossing columns: {curve2.multi_crossing_columns}")
                # show the actual d values around the multi-crossing column to see the shape
                for col_idx, count in curve1.multi_crossing_columns[:3]:
                    beta2_val = sol.grid.axis[col_idx]
                    column = field_grid[:, col_idx]
                    sign_changes = [(sol.grid.axis[i], float(column[i])) for i in range(len(column) - 1)
                                     if column[i] * column[i + 1] < 0]
                    print(f"    beta2={beta2_val:.4f} column crossings ({count}): "
                          f"sign changes near beta1={[f'{b:.4f}' for b, _ in sign_changes]}")
                for col_idx, count in curve2.multi_crossing_columns[:3]:
                    beta1_val = sol.grid.axis[col_idx]
                    column = field_grid.T[:, col_idx]
                    sign_changes = [(sol.grid.axis[i], float(column[i])) for i in range(len(column) - 1)
                                     if column[i] * column[i + 1] < 0]
                    print(f"    beta1={beta1_val:.4f} row crossings ({count}): "
                          f"sign changes near beta2={[f'{b:.4f}' for b, _ in sign_changes]}")
    print(f"  total multi-crossings this resolution: {total_multi}")
    return total_multi


def main() -> None:
    print("=== Verifying the candidate policy-level multi-crossing (task #63 follow-up) ===")
    results = {}
    for resolution in [30, 60, 100, 150]:
        results[resolution] = check(resolution, n_iters=2500)

    print("\n=== Summary ===")
    for res, n in results.items():
        print(f"  resolution={res}: {n} multi-crossings")

    print("\n=== Verdict ===")
    if all(n > 0 for n in results.values()):
        print("The multi-crossing is present at EVERY resolution tested (30/60/100/150) -- this is a")
        print("real, resolution-stable finding, not a discretization fluke. #42/#49's conclusion that")
        print("the routing policy never loses its threshold structure must be revised: a targeted")
        print("(not blind-random) search CAN construct a genuine policy-level multi-crossing.")
    elif results[150] > 0 and results[30] == 0:
        print("The multi-crossing only appears at finer resolution -- consistent with a real but")
        print("small-scale feature that coarse grids miss, similar to how Gap G1's field-level")
        print("counterexample needed higher resolution to fully resolve. Still real, not an artifact")
        print("(a resolution-DEPENDENT artifact would typically vanish at fine resolution, not appear).")
    else:
        print("Mixed/inconsistent results across resolution -- needs more careful interpretation before")
        print("treating this as a confirmed policy-level break. Report exactly what was found, honestly.")


if __name__ == "__main__":
    main()
