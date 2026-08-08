"""Adversarial check on ASYMMETRIC hop pairs (lambda1 != lambda2), per Fable-model review
priority order (2026-07-19): the c_switch_cold-axis monotonicity lemma is channel-parameter-
independent and safe to write anytime, but the lambda-axis finding (Phi(lambda) monotone
non-decreasing, symmetric pair, checked to lambda=0.99) is only an empirical observation on ONE
slice -- this project's own single-crossing saga died from exactly this kind of premature
confidence in a clean-looking symmetric/restricted slice. This script attacks the general
(lambda1, lambda2) surface directly, per Fable's prioritized attack list:

  1. Coarse (lambda1, lambda2) grid: test "Phi is coordinatewise non-decreasing in each lambda_i"
     (precise, falsifiable conjecture -- if it holds, the Phi=0 boundary is a monotone curve).
  2. Extreme asymmetry: one hop near-memoryless, the other highly persistent (connects to Stage 0
     Finding 2 -- how much does a memoryless hop2 dilute hop1's own persistence value?).
  3. Aggregation-hypothesis test: hold lambda1*lambda2 fixed, vary the split -- if Phi only
     depends on this product (or another single aggregate), the whole 2D structure collapses to
     1D (a major simplification); if not, the deviation IS the interesting structure.
  4. Persistence x contrast interaction (Fable's top suspect for where monotonicity breaks):
     raise lambda2 while simultaneously lowering hop2's contrast (loss_bad-loss_good).
  5. One lambda<0 (alternating) point, since real trace fits showed this structure exists.
  6. A degenerate-region mask overlaid on every sign map (cold-side switch rate == 0 means the
     comparison is trivial, not a real "warm vs cold" tradeoff).

Run with: uv run python warm_cold_asymmetric_attack_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

COST_A = 0.30
C_WARM = 0.02
C_SWITCH_WARM = 0.01
RESOLUTION = 50
N_ITERS = 1500


def hop_from_lambda(lam: float, pi_bad: float = 0.3, loss_good: float = 0.05, loss_bad: float = 0.5) -> channels.HopParams:
    p_gb = pi_bad * (1 - lam)
    p_bg = (1 - pi_bad) * (1 - lam)
    return channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=loss_good, eps_bad=loss_bad)


def stationary_path_b_loss(hop1: channels.HopParams, hop2: channels.HopParams) -> float:
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    pi_bad1 = hop1.p_gb / (hop1.p_gb + hop1.p_bg)
    pi_bad2 = hop2.p_gb / (hop2.p_gb + hop2.p_bg)
    pi = np.array([(1 - pi_bad1) * (1 - pi_bad2), (1 - pi_bad1) * pi_bad2,
                   pi_bad1 * (1 - pi_bad2), pi_bad1 * pi_bad2])
    return float((pi * path_b_loss).sum())


def phi(hop1: channels.HopParams, hop2: channels.HopParams, cost_a: float = COST_A,
        resolution: int = RESOLUTION, n_iters: int = N_ITERS) -> tuple[float, float]:
    """Returns (Phi, cold_switch_rate_at_plateau) -- the second value flags degenerate points."""
    plateau = min(cost_a, stationary_path_b_loss(hop1, hop2))
    sol_warm = switching_curves.always_warm_value_iteration(hop1, hop2, cost_a, C_WARM, C_SWITCH_WARM, resolution=resolution, n_iters=n_iters)
    # degenerate-region check: does always_warm's own belief-driven policy ever switch at all,
    # i.e. is this a genuine tradeoff or is path B categorically dominated/dominant regardless?
    switch_rate = float(np.mean(sol_warm.policy[:, 0] == 1)) if hasattr(sol_warm, "policy") else float("nan")
    return plateau - sol_warm.g, switch_rate


def probe1_coarse_grid() -> None:
    print("=== Probe 1: coarse (lambda1, lambda2) grid -- coordinatewise-monotonicity check ===\n")
    lambdas = [0.1, 0.3, 0.5, 0.65, 0.8, 0.95]
    grid = np.zeros((len(lambdas), len(lambdas)))
    for i, l1 in enumerate(lambdas):
        row = []
        for j, l2 in enumerate(lambdas):
            hop1, hop2 = hop_from_lambda(l1), hop_from_lambda(l2)
            p, _ = phi(hop1, hop2)
            grid[i, j] = p
            row.append(f"{p:+.4f}")
        print(f"lambda1={l1}: " + " ".join(row))

    print("\ncoordinatewise non-decreasing in lambda1 (down each column)?",
          all(np.all(np.diff(grid[:, j]) >= -1e-6) for j in range(grid.shape[1])))
    print("coordinatewise non-decreasing in lambda2 (across each row)?",
          all(np.all(np.diff(grid[i, :]) >= -1e-6) for i in range(grid.shape[0])))
    print()


def probe2_extreme_asymmetry() -> None:
    print("=== Probe 2: extreme asymmetry (one hop persistent, other near-memoryless) ===\n")
    for l1 in [0.9, 0.99]:
        for l2 in [0.0, 0.05, 0.1]:
            hop1, hop2 = hop_from_lambda(l1), hop_from_lambda(l2)
            p, switch_rate = phi(hop1, hop2)
            print(f"  lambda1={l1}, lambda2={l2}: Phi={p:+.4f}, always-warm switch rate={switch_rate:.3f}")
    print()


def probe3_aggregation_hypothesis() -> None:
    print("=== Probe 3: aggregation hypothesis -- fix lambda1*lambda2, vary the split ===\n")
    target_product = 0.5 * 0.5  # = 0.25
    splits = [(0.5, 0.5), (0.7, 0.25 / 0.7), (0.9, 0.25 / 0.9), (0.99, 0.25 / 0.99), (0.3, 0.25 / 0.3)]
    for l1, l2 in splits:
        if l2 > 0.999:
            continue
        hop1, hop2 = hop_from_lambda(l1), hop_from_lambda(l2)
        p, _ = phi(hop1, hop2)
        print(f"  lambda1={l1:.3f}, lambda2={l2:.3f} (product={l1*l2:.4f}): Phi={p:+.4f}")
    print("(if Phi were constant across this list, Phi would depend only on the product -- "
          "check if it is NOT constant, which would rule out this simplest aggregation)\n")


def probe4_persistence_contrast_interaction() -> None:
    print("=== Probe 4: persistence x contrast interaction (top suspect per Fable review) ===\n")
    l1 = 0.7
    hop1 = hop_from_lambda(l1)
    for l2 in [0.3, 0.5, 0.7, 0.9]:
        for contrast in [0.1, 0.3, 0.45]:  # loss_bad - loss_good, loss_good fixed at 0.05
            loss_bad2 = 0.05 + contrast
            hop2 = hop_from_lambda(l2, loss_good=0.05, loss_bad=loss_bad2)
            p, _ = phi(hop1, hop2)
            print(f"  lambda2={l2}, hop2_contrast={contrast:.2f} (loss_bad={loss_bad2:.2f}): Phi={p:+.4f}")
    print()


def probe5_alternating() -> None:
    print("=== Probe 5: one alternating (lambda<0) hop, matching real-trace-fit structure ===\n")
    for l1, l2 in [(-0.3, 0.7), (-0.5, 0.5), (-0.2, 0.9)]:
        hop1, hop2 = hop_from_lambda(l1), hop_from_lambda(l2)
        p, _ = phi(hop1, hop2)
        print(f"  lambda1={l1} (alternating), lambda2={l2}: Phi={p:+.4f}")
    print()


if __name__ == "__main__":
    probe1_coarse_grid()
    probe2_extreme_asymmetry()
    probe3_aggregation_hypothesis()
    probe4_persistence_contrast_interaction()
    probe5_alternating()
