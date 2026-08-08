"""Task #52 final step, part 2: place the real fitted hop pair
(`real_trace_ge_fit_demo.py`'s hop1/hop2, due/packet-delivery, distance=10m)
on the #56/#57 monotonicity-violation invariant
(max_voi_gap * contrast_product / lambda_product), and report where it falls
relative to the #47/#56 250-scenario training distribution (violators vs
non-violators) -- NOT as a classification (no fixed threshold was chosen;
#56/#57 only established rank-based AUC separation), but as an honest
"where does a real channel sit in this range" data point.

Run with: uv run python real_channel_invariant_check_demo.py
"""

from __future__ import annotations

import json

import numpy as np

from invariant_features_demo import extract_features

FEATURES_PATH = "output/invariant_features.json"

# Same real hop pair as real_channel_adaptivity_sweep_demo.py (due/packet-delivery, distance=10m).
REAL_PARAMS = dict(
    p_gb1=0.066, p_bg1=0.653, eps_good1=0.0, eps_bad1=1.0,  # lambda=+0.281, loss=0.092
    p_gb2=0.079, p_bg2=0.915, eps_good2=0.0, eps_bad2=1.0,  # lambda=+0.006, loss=0.079
    cost_a=0.16, c_warm=0.02, c_switch_warm=0.01, c_switch_cold=0.10,  # the peak point found
)


def main() -> None:
    feats = extract_features(REAL_PARAMS, resolution=60, n_iters=2000)
    eps = 1e-12
    invariant = feats["max_voi_gap"] * feats["contrast_product"] / (feats["lambda_product"] + eps)
    print("=== Real channel pair on the #56/#57 invariant (max_voi_gap*contrast_product/lambda_product) ===")
    print(f"lambda1={feats['lambda1']:+.4f}, lambda2={feats['lambda2']:+.4f}, "
          f"lambda_product={feats['lambda_product']:+.6f}")
    print(f"contrast1={feats['contrast1']:.4f}, contrast2={feats['contrast2']:.4f}, "
          f"contrast_product={feats['contrast_product']:.4f} (=1 exactly: both real hops are pure-Gilbert, "
          "an honest fact about this dataset, not a modeling choice)")
    print(f"max_voi_gap={feats['max_voi_gap']:.6e}")
    print(f"invariant value = {invariant:.4f}")

    with open(FEATURES_PATH) as f:
        data = json.load(f)
    records = data["records"]
    lam_prod = np.array([r["lambda_product"] for r in records])
    contrast_prod = np.array([r["contrast_product"] for r in records])
    voi_gap = np.array([r["max_voi_gap"] for r in records])
    labels = np.array([bool(r["is_violator"]) for r in records])
    train_invariant = voi_gap * contrast_prod / (lam_prod + eps)

    print(f"\n250-scenario training set (seed=12345, #47/#56): "
          f"{labels.sum()} violators / {len(labels)} total")
    print(f"  non-violator invariant: min={train_invariant[~labels].min():.4f}, "
          f"median={np.median(train_invariant[~labels]):.4f}, max={train_invariant[~labels].max():.4f}")
    print(f"  violator invariant:     min={train_invariant[labels].min():.4f}, "
          f"median={np.median(train_invariant[labels]):.4f}, max={train_invariant[labels].max():.4f}")
    percentile = float(np.mean(train_invariant <= invariant) * 100)
    percentile_nonviol = float(np.mean(train_invariant[~labels] <= invariant) * 100)
    print(f"\nreal-channel invariant value {invariant:.4f} sits at the "
          f"{percentile:.1f}th percentile of all 250 training scenarios "
          f"({percentile_nonviol:.1f}th percentile among non-violators alone).")

    print("\n=== Verdict ===")
    print("No fixed threshold was ever established (#56/#57 only validated rank-based AUC")
    print("separation, not a classification cutoff), so this is NOT a violation prediction --")
    print("it is a data point showing where a real, fitted channel pair happens to sit in the")
    print("range explored by the synthetic adversarial search.")


if __name__ == "__main__":
    main()
