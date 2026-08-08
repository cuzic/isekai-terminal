"""D1-1 (task #44): does the counterexample's non-monotone `d_field_full_model`
dip actually cross zero more than once on any beta1-column, i.e. does the
ROUTING POLICY itself stop being a threshold in beta1 -- or does `d` stay
non-monotone but still cross zero exactly once per column, so the policy
remains a clean threshold despite the field's non-monotonicity?

This is the gating check both external reviews (Codex CLI + Fable, 2026-07-18)
flagged as highest priority: a field can dip and recover without ever
crossing its own decision level (0, for `d_field_full_model` -- no +-c_switch
offset needed, see that function's docstring), in which case Gap G1's
non-monotonicity is a real but policy-irrelevant curiosity. If it DOES cross
zero more than once somewhere, the routing policy genuinely loses the
threshold structure there, which is a stronger (and more citable) finding.

Uses the exact witness parameters recovered from output/adversarial_search_log.json
(trial 90 of the seed=12345 run in adversarial_search_demo.py / task #43),
not the rounded values in THRESHOLD_PROOF.md's prose (Codex flagged that
text as inaccurate/partial -- see task #44's description).

Run with: uv run python zero_crossing_check_demo.py
"""

from __future__ import annotations

import json

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

LOG_PATH = "output/adversarial_search_log.json"


def extract_level_curve_beta2_direction(field_flat, grid, level: float):
    """`extract_level_curve` fixes beta2 per column and root-finds along
    beta1 (it iterates `field_grid.shape[1]` columns, each a beta1-slice).
    The counterexample's actual `check_monotone_grid` violations are on the
    *beta2* axis (0 on beta1-axis, >0 on beta2-axis -- see this script's
    printed output), so the threshold-structure question that actually
    matters here is the transposed one: for each FIXED beta1, does d cross
    zero more than once as beta2 varies? Since the grid is square
    (`grid.shape[0] == grid.shape[1]`), transposing the field before
    reshaping and reusing the same square `grid.axis` gives exactly that."""
    field_grid = field_flat.reshape(grid.shape)
    return switching_curves.extract_level_curve(field_grid.T.reshape(-1), grid, level)


def main() -> None:
    with open(LOG_PATH) as f:
        log = json.load(f)
    params = log["worst"]["params"]
    print("=== Witness parameters (trial 90, seed=12345, from output/adversarial_search_log.json) ===")
    for k, v in params.items():
        print(f"  {k} = {v!r}")

    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )

    print("\n=== Re-solving at higher resolution (150) for a clean level-curve extraction ===")
    resolution = 150
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=2000)

    any_multi_crossing_beta1 = False
    any_multi_crossing_beta2 = False
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            mono = switching_curves.check_monotone_grid(d_full, sol.grid)
            curve1 = switching_curves.extract_level_curve(d_full, sol.grid, level=0.0)
            curve2 = extract_level_curve_beta2_direction(d_full, sol.grid, level=0.0)
            print(f"\ncontext (p={p}, w={w}):")
            print(f"  d-field monotonicity violations: beta1-axis={mono['n_violations_beta1']}"
                  f" (max {mono['max_violation_beta1']:.4e}),"
                  f" beta2-axis={mono['n_violations_beta2']} (max {mono['max_violation_beta2']:.4e})")
            print(f"  [fixed-beta2, root over beta1] columns: single-crossing={len(curve1.beta2)}, "
                  f"no-crossing={len(curve1.no_crossing_columns)}, "
                  f"multi-crossing={len(curve1.multi_crossing_columns)}")
            print(f"  [fixed-beta1, root over beta2] rows:    single-crossing={len(curve2.beta2)}, "
                  f"no-crossing={len(curve2.no_crossing_columns)}, "
                  f"multi-crossing={len(curve2.multi_crossing_columns)}")
            if curve1.multi_crossing_columns:
                any_multi_crossing_beta1 = True
                print(f"  MULTI-CROSSING (fixed-beta2, over beta1) (index, count): "
                      f"{curve1.multi_crossing_columns[:20]}"
                      f"{' ...' if len(curve1.multi_crossing_columns) > 20 else ''}")
            if curve2.multi_crossing_columns:
                any_multi_crossing_beta2 = True
                print(f"  MULTI-CROSSING (fixed-beta1, over beta2) (index, count): "
                      f"{curve2.multi_crossing_columns[:20]}"
                      f"{' ...' if len(curve2.multi_crossing_columns) > 20 else ''}")

    print("\n=== Verdict ===")
    print(f"beta1-direction (fixed beta2, threshold over beta1): "
          f"{'BROKEN (multi-crossing found)' if any_multi_crossing_beta1 else 'threshold holds everywhere'}")
    print(f"beta2-direction (fixed beta1, threshold over beta2): "
          f"{'BROKEN (multi-crossing found)' if any_multi_crossing_beta2 else 'threshold holds everywhere'}")
    if not any_multi_crossing_beta1 and not any_multi_crossing_beta2:
        print("\nNeither direction shows a multi-crossing column/row despite d_field_full_model itself")
        print("being non-monotone (violations concentrated on the beta2 axis). The dip changes the")
        print("field's magnitude/slope locally but never flips which side of zero a fixed slice sits")
        print("on. So at this counterexample, the ROUTING POLICY remains a clean single-threshold")
        print("curve in each direction even though the underlying Q-difference field is not monotone --")
        print("Gap G1's finding is about the FIELD's monotonicity, not the POLICY's threshold")
        print("structure. #42's paper text should state this distinction explicitly rather than")
        print("implying non-monotone d automatically means a non-threshold policy.")
    else:
        print("\nAt least one direction has a genuine multi-crossing: the ROUTING POLICY itself loses")
        print("the threshold structure there, not just the underlying field -- a stronger finding.")


if __name__ == "__main__":
    main()
