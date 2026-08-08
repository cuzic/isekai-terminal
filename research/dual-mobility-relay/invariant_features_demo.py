"""D1-4 phase (a) (task #47): build a per-scenario feature table from the
full-logging adversarial search (#43's `output/adversarial_search_log.json`),
for #56 to build candidate invariants from that predict monotonicity
violation.

Candidate features per scenario:
  - lambda1, lambda2: per-hop persistence, 1 - p_gb - p_bg (mean sojourn
    time is 1/p_gb, 1/p_bg in the two states; lambda near 1 means very
    "sticky"/slow-mixing, near 0 means fast-mixing).
  - contrast1, contrast2: per-hop loss contrast, eps_bad - eps_good (how
    much the loss rate actually depends on the hidden state -- zero
    contrast means the hidden state carries no decision-relevant
    information at all).
  - max_voi_gap: the solve-derived one-step value-of-information surrogate
    J(beta) = h(predict(beta)) - E_o[h(posterior(beta))], maximized over the
    belief grid and over all 4 `(p,w)` contexts. NOT implemented anywhere
    else in this codebase -- `dmr/voi.py`'s `bayes_risk` is the exact
    one-shot Blackwell gap for a FIXED prior/Q-table, a different quantity
    from this solve-derived, belief-dependent gap (see this project's
    THRESHOLD_PROOF.md §4 / `d_field_full_model`'s docstring on why this
    term is hump-shaped and breaks Topkis' increasing-differences
    condition). Reuses `beliefgrid2d.predict_scalar` / `obs_prob_scalar` /
    `bayes_update_scalar` / `RegularGrid2D.interpolate_batch` -- the same
    primitives `beliefgrid2d._continuation` already combines internally,
    just exposed here as a standalone diagnostic rather than folded into a
    Bellman backup.

Output: `output/invariant_features.json`, a list of per-trial records with
these features plus the existing `total_viol`/`max_viol_mag` labels from
`adversarial_search_demo.py`'s output, for #56's invariant-hypothesis stage.

Run with: uv run python invariant_features_demo.py   (~3 min: re-solves all
250 scenarios at resolution=30 to get the value function needed for the VoI
feature, since #43's log only stored total_viol, not h)
"""

from __future__ import annotations

import json

import numpy as np

from dmr import beliefgrid2d, channels, warm_standby

IN_LOG_PATH = "output/adversarial_search_log.json"
OUT_PATH = "output/invariant_features.json"


def voi_gap(grid: beliefgrid2d.RegularGrid2D, hop1: channels.HopParams, hop2: channels.HopParams,
            h_slice: np.ndarray) -> np.ndarray:
    """J(beta) = h(predict(beta)) - E_o[h(posterior-then-predict(beta,o))],
    at every grid point at once, for a fixed context's value slice
    `h_slice`. By Jensen (h separately concave in each coordinate) this is
    >= 0 everywhere; it is exactly the "value of observing before predicting"
    term that THRESHOLD_PROOF.md's Gap G1 discussion argues is hump-shaped
    in beta and responsible for breaking Topkis' increasing-differences
    condition for the full (action-dependent-observability) model."""
    b1, b2 = grid.beta1, grid.beta2
    b1_pred = beliefgrid2d.predict_scalar(b1, hop1)
    b2_pred = beliefgrid2d.predict_scalar(b2, hop2)
    predict_only = grid.interpolate_batch(h_slice, b1_pred, b2_pred)

    bayes_observed = np.zeros(grid.n_points)
    for l1 in (0, 1):
        p1 = beliefgrid2d.obs_prob_scalar(b1, hop1, l1)
        b1_next = beliefgrid2d.predict_scalar(beliefgrid2d.bayes_update_scalar(b1, hop1, l1), hop1)
        for l2 in (0, 1):
            p2 = beliefgrid2d.obs_prob_scalar(b2, hop2, l2)
            b2_next = beliefgrid2d.predict_scalar(beliefgrid2d.bayes_update_scalar(b2, hop2, l2), hop2)
            interp_vals = grid.interpolate_batch(h_slice, b1_next, b2_next)
            bayes_observed += p1 * p2 * interp_vals

    return predict_only - bayes_observed


def extract_features(params: dict, resolution: int = 30, n_iters: int = 800) -> dict:
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)

    max_voi_gap = 0.0
    for p in range(2):
        for w in range(2):
            gap = voi_gap(sol.grid, hop1, hop2, sol.h[:, p, w])
            max_voi_gap = max(max_voi_gap, float(np.max(gap)))

    lambda1 = 1.0 - params["p_gb1"] - params["p_bg1"]
    lambda2 = 1.0 - params["p_gb2"] - params["p_bg2"]
    contrast1 = params["eps_bad1"] - params["eps_good1"]
    contrast2 = params["eps_bad2"] - params["eps_good2"]

    return dict(
        lambda1=lambda1, lambda2=lambda2, lambda_product=lambda1 * lambda2,
        contrast1=contrast1, contrast2=contrast2, contrast_product=contrast1 * contrast2,
        max_voi_gap=max_voi_gap,
    )


def main() -> None:
    with open(IN_LOG_PATH) as f:
        log = json.load(f)

    records = []
    for t in log["trials"]:
        if "error" in t:
            continue
        try:
            feats = extract_features(t["params"])
        except Exception as exc:
            print(f"trial {t['trial']}: feature extraction FAILED ({exc})")
            continue
        rec = {
            "trial": t["trial"],
            "total_viol": t["total_viol"],
            "max_viol_mag": t["max_viol_mag"],
            "is_violator": t["total_viol"] > 0,
            **feats,
        }
        records.append(rec)
        if rec["is_violator"]:
            print(f"trial {t['trial']:>3} [VIOLATOR, viol={t['total_viol']:>4}]: "
                  f"lambda1={feats['lambda1']:.3f}, lambda2={feats['lambda2']:.3f}, "
                  f"contrast1={feats['contrast1']:.3f}, contrast2={feats['contrast2']:.3f}, "
                  f"max_voi_gap={feats['max_voi_gap']:.4e}")

    with open(OUT_PATH, "w") as f:
        json.dump({"seed": log["seed"], "records": records}, f)
    print(f"\nwrote {OUT_PATH} ({len(records)} records)")

    violators = [r for r in records if r["is_violator"]]
    non_violators = [r for r in records if not r["is_violator"]]
    print(f"\n{len(violators)} violators, {len(non_violators)} non-violators")
    for feat in ["lambda1", "lambda2", "lambda_product", "contrast1", "contrast2",
                 "contrast_product", "max_voi_gap"]:
        v_mean = np.mean([r[feat] for r in violators])
        nv_mean = np.mean([r[feat] for r in non_violators])
        print(f"  {feat}: violator mean={v_mean:.4e}, non-violator mean={nv_mean:.4e}, "
              f"ratio={v_mean / nv_mean if nv_mean else float('nan'):.3f}")


if __name__ == "__main__":
    main()
