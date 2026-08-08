"""D1-5 (task #48): does a monotonicity counterexample exist for the always-
cold sub-model's routing d-field, not just the full (4-action) model's?

`switching_curves.always_cold_value_iteration` is the purest form of Gap
G1's obstruction: 2 routing actions (A, B), standby never warmed, and
action-dependent observability (context A/cold never observes, context B/
live-traffic always does) with no warm/cold choice at all layered on top.
If a counterexample exists here too, the mechanism analysis (task #47/#56)
can proceed in a far simpler 2-context model; if none is found despite the
same observability asymmetry being present, that itself is informative --
it would implicate the warm/cold CHOICE specifically (the full model's extra
action dimension), not observability asymmetry alone, as necessary for the
break found in `adversarial_search_demo.py`.

`AlwaysColdSolution` doesn't store the `base_a`/`base_b` continuation values
(only `h`/`policy`), so this script recomputes them the same way
`always_cold_value_iteration` does internally, via the same private
`_continuation_always_cold` helper -- this needed no new public API since
`d = base_b - base_a` is context-independent (both `base_a` and `base_b` are
computed once per solve, before the context-A/context-B switch-cost split),
matching `d_field_full_model`'s docstring's account of why the always-warm
proof doesn't transfer to any model with action-dependent observability.

Run with: uv run python always_cold_adversarial_search_demo.py
"""

from __future__ import annotations

import json

import numpy as np

from dmr import channels, switching_curves
from dmr.switching_curves import CONTEXT_A, CONTEXT_B, RegularGrid2D, _continuation_always_cold

LOG_PATH = "output/always_cold_adversarial_search_log.json"


def random_scenario(rng: np.random.Generator) -> dict:
    p_gb1 = 10 ** rng.uniform(-2.5, -0.7)
    p_bg1 = 10 ** rng.uniform(-1.5, -0.1)
    eps_good1 = rng.uniform(0.001, 0.05)
    eps_bad1 = rng.uniform(eps_good1 + 0.02, 0.95)

    p_gb2 = 10 ** rng.uniform(-2.5, -0.7)
    p_bg2 = 10 ** rng.uniform(-1.5, -0.1)
    eps_good2 = rng.uniform(0.001, 0.05)
    eps_bad2 = rng.uniform(eps_good2 + 0.02, 0.95)

    cost_a = rng.uniform(0.02, 0.3)
    c_switch_cold = 10 ** rng.uniform(-2, 0)

    return dict(
        p_gb1=p_gb1, p_bg1=p_bg1, eps_good1=eps_good1, eps_bad1=eps_bad1,
        p_gb2=p_gb2, p_bg2=p_bg2, eps_good2=eps_good2, eps_bad2=eps_bad2,
        cost_a=cost_a, c_switch_cold=c_switch_cold,
    )


def d_field_always_cold(params: dict, resolution: int, n_iters: int = 2000):
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    sol = switching_curves.always_cold_value_iteration(
        hop1, hop2, params["cost_a"], params["c_switch_cold"], resolution=resolution, n_iters=n_iters
    )
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    content_a = np.full(sol.grid.n_points, params["cost_a"])
    content_b = sol.grid.joint_probs() @ path_b_loss
    cont_a = _continuation_always_cold(sol.grid, hop1, hop2, sol.h[:, CONTEXT_A], observable=False)
    cont_b = _continuation_always_cold(sol.grid, hop1, hop2, sol.h[:, CONTEXT_B], observable=True)
    base_a = content_a + cont_a
    base_b = content_b + cont_b
    return base_b - base_a, sol


def monotonicity_violations(params: dict, resolution: int) -> tuple[int, float]:
    d, sol = d_field_always_cold(params, resolution=resolution)
    mono = switching_curves.check_monotone_grid(d, sol.grid)
    total = mono["n_violations_beta1"] + mono["n_violations_beta2"]
    max_mag = max(mono["max_violation_beta1"], mono["max_violation_beta2"])
    return total, max_mag


def main() -> None:
    rng = np.random.default_rng(12345)
    n_trials = 250
    worst = {"total_viol": 0, "max_viol_mag": 0.0, "params": None}
    all_trials: list[dict] = []

    print(f"=== Always-cold adversarial search: {n_trials} random scenarios ===")
    for trial in range(n_trials):
        params = random_scenario(rng)
        try:
            total_viol, max_mag = monotonicity_violations(params, resolution=30)
        except Exception as exc:
            all_trials.append({"trial": trial, "params": params, "error": str(exc)})
            continue
        all_trials.append({"trial": trial, "params": params, "total_viol": total_viol, "max_viol_mag": max_mag})
        if total_viol > worst["total_viol"]:
            worst = {"total_viol": total_viol, "max_viol_mag": max_mag, "params": params}
            print(f"trial {trial}: new worst = {total_viol} violations, max magnitude {max_mag:.4e}")

    n_violators = sum(1 for t in all_trials if t.get("total_viol", 0) > 0)
    n_errored = sum(1 for t in all_trials if "error" in t)
    print(f"\nprevalence: {n_violators}/{len(all_trials)} trials showed at least one monotonicity "
          f"violation ({n_errored} trials errored and were excluded)")
    print(f"worst case found: {worst['total_viol']} violations, magnitude {worst['max_viol_mag']:.4e}")
    print(f"params: {worst['params']}")

    with open(LOG_PATH, "w") as f:
        json.dump({"seed": 12345, "n_trials": n_trials, "trials": all_trials, "worst": worst}, f)
    print(f"wrote {LOG_PATH}")

    print("\n=== Verdict ===")
    if worst["total_viol"] == 0:
        print("No counterexample found in the always-cold sub-model despite the same action-")
        print("dependent-observability obstruction being present. This implicates the warm/cold")
        print("CHOICE itself (the full model's extra action dimension) as necessary for the break")
        print("found in adversarial_search_demo.py -- observability asymmetry alone is not enough.")
    else:
        print("A counterexample exists in the always-cold sub-model too -- confirming action-")
        print("dependent observability alone (without any warm/cold choice) is sufficient to break")
        print("monotonicity. Re-running the resolution-convergence check on the worst case:")
        for resolution in [30, 60, 100, 150]:
            total_viol, max_mag = monotonicity_violations(worst["params"], resolution=resolution)
            print(f"resolution={resolution:>3}: violations={total_viol:>5}, magnitude={max_mag:.4e}, "
                  f"magnitude*resolution={max_mag * resolution:.4f}")


if __name__ == "__main__":
    main()
