"""D1-4b (task #56): using #47's feature table (output/invariant_features.json,
250 scenarios from the seed=12345 adversarial search, 12 violators), construct
and rank candidate scalar invariants that predict monotonicity violation --
going beyond `lambda1*lambda2` (already shown, in #47's descriptive means, to
be backwards on average: violators have LOWER lambda_product, not higher).

No scikit-learn/pandas in this project's dependencies (see pyproject.toml --
numpy/scipy/matplotlib only), so this uses a from-scratch rank-based AUC
(Mann-Whitney U statistic, robust to the severe 12-vs-238 class imbalance --
no threshold needs to be chosen up front) to score each candidate, rather
than fitting a classifier. This is deliberately simple: the point is to find
which SIMPLE combination of already-extracted features separates violators
from non-violators, not to fit a black-box model to 12 positive examples.

Candidates tried (each just a product/ratio of #47's already-extracted
features, since 12 positives is far too few to fit anything with more than
a couple of free combinations):
  - lambda_product              (the naive baseline this task must beat)
  - contrast_product
  - max_voi_gap
  - max_voi_gap / lambda_product
  - contrast_product / lambda_product
  - max_voi_gap * contrast_product / lambda_product

The winning candidate (highest AUC) is reported as THIS session's leading
invariant hypothesis -- #57's job is to validate it on a fresh, independently-
seeded search (not reuse this same 250-scenario sample it was chosen from,
which would be circular).

Run with: uv run python invariant_candidates_demo.py
"""

from __future__ import annotations

import json

import numpy as np

FEATURES_PATH = "output/invariant_features.json"


def auc(scores: np.ndarray, labels: np.ndarray) -> float:
    """Rank-based AUC (Mann-Whitney U / (n_pos*n_neg)). 0.5 = no separation,
    1.0 = positives always score above negatives, 0.0 = always below."""
    order = np.argsort(scores)
    ranks = np.empty_like(order, dtype=float)
    ranks[order] = np.arange(1, len(scores) + 1)
    # average ranks for ties
    sorted_scores = scores[order]
    i = 0
    while i < len(sorted_scores):
        j = i
        while j + 1 < len(sorted_scores) and sorted_scores[j + 1] == sorted_scores[i]:
            j += 1
        if j > i:
            avg_rank = ranks[order[i:j + 1]].mean()
            ranks[order[i:j + 1]] = avg_rank
        i = j + 1
    n_pos = int(labels.sum())
    n_neg = len(labels) - n_pos
    sum_ranks_pos = ranks[labels].sum()
    u = sum_ranks_pos - n_pos * (n_pos + 1) / 2
    return float(u / (n_pos * n_neg))


def main() -> None:
    with open(FEATURES_PATH) as f:
        data = json.load(f)
    records = data["records"]

    lambda_product = np.array([r["lambda_product"] for r in records])
    contrast_product = np.array([r["contrast_product"] for r in records])
    max_voi_gap = np.array([r["max_voi_gap"] for r in records])
    labels = np.array([r["is_violator"] for r in records])

    eps = 1e-12
    candidates = {
        "lambda_product": lambda_product,
        "contrast_product": contrast_product,
        "max_voi_gap": max_voi_gap,
        "max_voi_gap / lambda_product": max_voi_gap / (lambda_product + eps),
        "contrast_product / lambda_product": contrast_product / (lambda_product + eps),
        "max_voi_gap * contrast_product / lambda_product": max_voi_gap * contrast_product / (lambda_product + eps),
    }

    print(f"n={len(records)} ({int(labels.sum())} violators, {len(labels) - int(labels.sum())} non-violators)\n")
    results = []
    for name, scores in candidates.items():
        a = auc(scores, labels)
        # AUC < 0.5 means the candidate is inversely predictive -- report
        # the "oriented" AUC (max(a, 1-a)) alongside the raw direction.
        oriented = max(a, 1 - a)
        direction = "high=violator" if a >= 0.5 else "LOW=violator"
        results.append((name, a, oriented, direction))
        print(f"  {name:55s}: AUC={a:.3f}  (oriented={oriented:.3f}, {direction})")

    best = max(results, key=lambda r: r[2])
    print(f"\n=== Best candidate: {best[0]} (oriented AUC={best[2]:.3f}, {best[3]}) ===")
    print("AUC=0.5 is no better than random; 1.0 is perfect separation. This is the leading")
    print("invariant hypothesis to validate on a FRESH, independently-seeded search (task #57) --")
    print("not on this same 250-scenario sample it was chosen from, which would be circular.")


if __name__ == "__main__":
    main()
