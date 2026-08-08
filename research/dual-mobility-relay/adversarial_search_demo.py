"""Adversarial search for a counterexample to the full unconstrained model's
routing-threshold monotonicity (THRESHOLD_PROOF.md §4, Gap G1), plus a
resolution-convergence check confirming any counterexample found is a real
non-monotonicity in the continuous field, not a discretization artifact.

This is the gating diagnostic both external reviews (Codex CLI + an
independent Fable-model agent, 2026-07-18) recommended running *before*
attempting to prove a general sufficient condition for monotonicity: a
found counterexample means no such theorem exists, and the search itself
is cheaper than a doomed proof attempt.

Run with: uv run python adversarial_search_demo.py   (~2-3 min for the
search; the resolution-convergence re-check of the worst case adds ~1 min)
"""

from __future__ import annotations

import json

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

LOG_PATH = "output/adversarial_search_log.json"


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
    c_warm = 10 ** rng.uniform(-3, -0.5)
    c_switch_warm = 10 ** rng.uniform(-3, -1)
    c_switch_cold = c_switch_warm + 10 ** rng.uniform(-2, 0)

    return dict(
        p_gb1=p_gb1, p_bg1=p_bg1, eps_good1=eps_good1, eps_bad1=eps_bad1,
        p_gb2=p_gb2, p_bg2=p_bg2, eps_good2=eps_good2, eps_bad2=eps_bad2,
        cost_a=cost_a, c_warm=c_warm, c_switch_warm=c_switch_warm, c_switch_cold=c_switch_cold,
    )


def monotonicity_violations(params: dict, resolution: int, n_iters: int = 800) -> tuple[int, float, float]:
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)
    total_viol, max_mag = 0, 0.0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            mono = switching_curves.check_monotone_grid(d_full, sol.grid)
            total_viol += mono["n_violations_beta1"] + mono["n_violations_beta2"]
            max_mag = max(max_mag, mono["max_violation_beta1"], mono["max_violation_beta2"])
    return total_viol, max_mag, sol.g


def main() -> None:
    rng = np.random.default_rng(12345)
    n_trials = 250
    worst = {"total_viol": 0, "max_viol_mag": 0.0, "params": None}
    all_trials: list[dict] = []

    print(f"=== Adversarial search: {n_trials} random scenarios, trying to break routing monotonicity ===")
    print(f"(logging every trial's result to {LOG_PATH} -- earlier versions of this script only")
    print(" kept the running worst case, discarding every other trial's data; per external review")
    print(" 2026-07-18, full logging turns a single witness into a labeled violator/non-violator")
    print(" dataset for later invariant-hunting, at essentially no extra cost)")
    for trial in range(n_trials):
        params = random_scenario(rng)
        try:
            total_viol, max_mag, g = monotonicity_violations(params, resolution=30)
        except Exception as exc:
            all_trials.append({"trial": trial, "params": params, "error": str(exc)})
            continue
        all_trials.append({
            "trial": trial, "params": params,
            "total_viol": total_viol, "max_viol_mag": max_mag, "g": g,
        })
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

    if worst["total_viol"] == 0:
        print("\nNo counterexample found in this run -- monotonicity held everywhere tried.")
        return

    print("\n=== Resolution-convergence check: is this a real non-monotonicity, or a grid artifact? ===")
    print("(a genuine artifact has magnitude*resolution -> 0; a real finite-slope dip has it converge)")
    for resolution in [30, 60, 100, 150]:
        total_viol, max_mag, g = monotonicity_violations(worst["params"], resolution=resolution, n_iters=2000)
        print(f"resolution={resolution:>3}: violations={total_viol:>5}, magnitude={max_mag:.4e}, "
              f"magnitude*resolution={max_mag*resolution:.4f}, g={g:.5f}")


if __name__ == "__main__":
    main()
