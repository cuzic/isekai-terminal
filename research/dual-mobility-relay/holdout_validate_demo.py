"""D1-4c (task #57): validate #56's leading candidate invariant --
`max_voi_gap * contrast_product / lambda_product`, oriented AUC 0.894 on the
seed=12345 sample it was chosen from -- against a FRESH, independently-
seeded random search (seed=99999, same sampling distributions as
`adversarial_search_demo.py`'s `random_scenario`, but a disjoint draw). This
is the check that separates a real signal from an artifact of fitting to
the one sample it was mined from.

Solves each of the 250 new scenarios ONCE (not twice), computing both the
monotonicity-violation label (matching `adversarial_search_demo.py`'s
`monotonicity_violations`) and the candidate-invariant features (matching
`invariant_features_demo.py`'s `extract_features`) from the same solve, to
avoid doubling the resolve cost of a fresh 250-scenario sweep.

Run with: uv run python holdout_validate_demo.py   (~3 min)
"""

from __future__ import annotations

import json

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby
from invariant_features_demo import voi_gap
from invariant_candidates_demo import auc

OUT_PATH = "output/holdout_validation_log.json"
HOLDOUT_SEED = 99999


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


def solve_trial(params: dict, resolution: int = 30, n_iters: int = 800) -> dict:
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)

    total_viol = 0
    max_voi = 0.0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            mono = switching_curves.check_monotone_grid(d_full, sol.grid)
            total_viol += mono["n_violations_beta1"] + mono["n_violations_beta2"]
            gap = voi_gap(sol.grid, hop1, hop2, sol.h[:, p, w])
            max_voi = max(max_voi, float(np.max(gap)))

    lambda1 = 1.0 - params["p_gb1"] - params["p_bg1"]
    lambda2 = 1.0 - params["p_gb2"] - params["p_bg2"]
    contrast1 = params["eps_bad1"] - params["eps_good1"]
    contrast2 = params["eps_bad2"] - params["eps_good2"]
    return dict(
        total_viol=total_viol, is_violator=total_viol > 0,
        lambda_product=lambda1 * lambda2, contrast_product=contrast1 * contrast2, max_voi_gap=max_voi,
    )


def main() -> None:
    rng = np.random.default_rng(HOLDOUT_SEED)
    n_trials = 250
    records = []
    for trial in range(n_trials):
        params = random_scenario(rng)
        try:
            rec = solve_trial(params)
        except Exception as exc:
            print(f"trial {trial}: ERROR ({exc})")
            continue
        rec["trial"] = trial
        records.append(rec)
        if rec["is_violator"]:
            print(f"trial {trial:>3} [VIOLATOR, viol={rec['total_viol']:>4}]: "
                  f"lambda_product={rec['lambda_product']:.3f}, "
                  f"contrast_product={rec['contrast_product']:.3f}, max_voi_gap={rec['max_voi_gap']:.4e}")

    with open(OUT_PATH, "w") as f:
        json.dump({"seed": HOLDOUT_SEED, "n_trials": n_trials, "records": records}, f)

    n_violators = sum(1 for r in records if r["is_violator"])
    print(f"\n{n_violators}/{len(records)} holdout trials are violators (training sample: 12/250)")

    eps = 1e-12
    lambda_product = np.array([r["lambda_product"] for r in records])
    contrast_product = np.array([r["contrast_product"] for r in records])
    max_voi_gap = np.array([r["max_voi_gap"] for r in records])
    labels = np.array([r["is_violator"] for r in records])

    candidate_score = max_voi_gap * contrast_product / (lambda_product + eps)
    a = auc(candidate_score, labels)
    print(f"\nHOLDOUT AUC of 'max_voi_gap * contrast_product / lambda_product': {a:.3f}")
    print(f"(training-sample AUC was 0.894 -- see invariant_candidates_demo.py)")

    # also recheck the simpler single-feature candidates for comparison
    for name, scores in [("lambda_product", lambda_product), ("contrast_product", contrast_product),
                          ("max_voi_gap", max_voi_gap)]:
        a2 = auc(scores, labels)
        print(f"  holdout AUC {name}: {a2:.3f} (oriented {max(a2, 1 - a2):.3f})")

    print("\n=== Verdict ===")
    if a > 0.75:
        print("The candidate invariant generalizes well to the fresh holdout sample -- a real,")
        print("non-circular signal, not an artifact of the 12-witness training sample.")
    elif a > 0.6:
        print("The candidate invariant shows a real but weaker signal on holdout data than on the")
        print("training sample (0.894) -- some overfitting to the training sample, but not pure noise.")
    else:
        print("The candidate invariant does NOT generalize to the fresh holdout sample -- the training-")
        print("sample AUC of 0.894 was likely an artifact of fitting to only 12 positive examples.")
        print("No validated invariant exists; report this honestly rather than keeping the 0.894 figure.")


if __name__ == "__main__":
    main()
