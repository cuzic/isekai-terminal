"""Per Fable-model review (2026-07-19), the 3 remaining checks before stopping this research
thread and writing it up (explicit stop rule, to avoid repeating the single-crossing saga's
"just one more targeted search" spiral):

1. MEAN-PRESERVING SPREAD: probe4's apparent persistence x contrast non-monotonicity in
   warm_cold_asymmetric_attack_demo.py varied hop2's contrast (loss_bad-loss_good) while holding
   loss_good FIXED at 0.05 -- this also shifts hop2's stationary average loss upward, which could
   mechanically hit the min(cost_a, stationary_path_b_loss) kink in Phi's own definition, faking
   a "value of information" story that's really just a mean-shift artifact. This script instead
   holds hop2's stationary average loss CONSTANT while varying contrast, isolating the pure
   information-structure effect.
2. DEGENERATE-BOUNDARY OVERLAP CHECK: does the sign-flip location (from the ORIGINAL,
   non-mean-preserving probe4) coincide with where stationary path-B loss crosses cost_a (the
   min()-kink line)? If yes, that's strong evidence the original wobble is (at least partly) the
   mechanical kink, not pure information value.
3. ONE (lambda2, contrast2) SIGN MAP, with a degenerate-region mask -- the paper figure.

After these 3, STOP per the explicit stop rule -- do not keep refining indefinitely.

Run with: uv run python warm_cold_mechanism_check_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

COST_A = 0.30
C_WARM = 0.02
C_SWITCH_WARM = 0.01
RESOLUTION = 50
N_ITERS = 1500
PI_BAD = 0.3


def hop_from_lambda(lam: float, loss_good: float = 0.05, loss_bad: float = 0.5, pi_bad: float = PI_BAD) -> channels.HopParams:
    p_gb = pi_bad * (1 - lam)
    p_bg = (1 - pi_bad) * (1 - lam)
    return channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=loss_good, eps_bad=loss_bad)


def hop_from_lambda_mean_preserving(lam: float, contrast: float, target_mean: float, pi_bad: float = PI_BAD) -> channels.HopParams:
    """loss_good/loss_bad chosen so pi_bad*loss_bad + (1-pi_bad)*loss_good == target_mean
    EXACTLY, for the given contrast = loss_bad - loss_good."""
    loss_good = target_mean - pi_bad * contrast
    loss_bad = loss_good + contrast
    if loss_good < 0:
        raise ValueError(f"loss_good would be negative ({loss_good:.4f}) at contrast={contrast}, target_mean={target_mean}")
    return hop_from_lambda(lam, loss_good=loss_good, loss_bad=loss_bad, pi_bad=pi_bad)


def stationary_path_b_loss(hop1: channels.HopParams, hop2: channels.HopParams) -> float:
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    pi_bad1 = hop1.p_gb / (hop1.p_gb + hop1.p_bg)
    pi_bad2 = hop2.p_gb / (hop2.p_gb + hop2.p_bg)
    pi = np.array([(1 - pi_bad1) * (1 - pi_bad2), (1 - pi_bad1) * pi_bad2,
                   pi_bad1 * (1 - pi_bad2), pi_bad1 * pi_bad2])
    return float((pi * path_b_loss).sum())


def phi(hop1: channels.HopParams, hop2: channels.HopParams, resolution: int = RESOLUTION, n_iters: int = N_ITERS) -> tuple[float, float]:
    plateau = min(COST_A, stationary_path_b_loss(hop1, hop2))
    sol_warm = switching_curves.always_warm_value_iteration(hop1, hop2, COST_A, C_WARM, C_SWITCH_WARM, resolution=resolution, n_iters=n_iters)
    switch_rate = float(np.mean(sol_warm.policy[:, 0] == 1))
    return plateau - sol_warm.g, switch_rate, stationary_path_b_loss(hop1, hop2)


def check1_mean_preserving_spread() -> None:
    print("=== Check 1: mean-preserving spread (isolates pure information-structure effect) ===\n")
    l1 = 0.7
    hop1 = hop_from_lambda(l1)
    target_mean = PI_BAD * 0.35 + (1 - PI_BAD) * 0.05  # = 0.14, matches probe4's contrast=0.30 baseline
    contrasts = [0.05, 0.10, 0.20, 0.30, 0.40, 0.45]
    for l2 in [0.3, 0.5, 0.7, 0.9]:
        row = []
        for c in contrasts:
            hop2 = hop_from_lambda_mean_preserving(l2, c, target_mean)
            p, _, _ = phi(hop1, hop2)
            row.append(f"{p:+.4f}")
        print(f"lambda2={l2} (mean fixed at {target_mean:.3f}): " + " ".join(f"c={c:.2f}:{v}" for c, v in zip(contrasts, row)))
    print("\n(if non-monotone here too -> real information-value effect; if monotone/flat -> the")
    print(" original probe4 wobble was (at least largely) the mean-shift/kink artifact)\n")


def check2_degenerate_boundary_overlap() -> None:
    print("=== Check 2: does the ORIGINAL (non-mean-preserving) sign-flip coincide with path_b_loss=cost_a? ===\n")
    l1 = 0.7
    hop1 = hop_from_lambda(l1)
    for l2 in [0.3, 0.5, 0.7, 0.9]:
        print(f"lambda2={l2}:")
        for contrast in np.arange(0.05, 0.50, 0.05):
            loss_bad2 = 0.05 + contrast
            hop2 = hop_from_lambda(l2, loss_good=0.05, loss_bad=loss_bad2)
            p, switch_rate, path_b = phi(hop1, hop2)
            kink_marker = " <-- path_b_loss crosses cost_a here" if abs(path_b - COST_A) < 0.02 else ""
            print(f"  contrast={contrast:.2f}: Phi={p:+.4f}, stationary_path_b_loss={path_b:.4f}{kink_marker}")
        print()


def check3_sign_map() -> None:
    print("=== Check 3: (lambda2, contrast2) Phi sign map with degenerate-region mask ===\n")
    l1 = 0.7
    hop1 = hop_from_lambda(l1)
    lambda2_values = [0.1, 0.3, 0.5, 0.7, 0.9]
    contrast_values = [0.05, 0.15, 0.25, 0.35, 0.45]
    print(f"{'':>12}" + "".join(f"c={c:.2f}".rjust(10) for c in contrast_values))
    for l2 in lambda2_values:
        row_phi = []
        row_mask = []
        for c in contrast_values:
            loss_bad2 = 0.05 + c
            hop2 = hop_from_lambda(l2, loss_good=0.05, loss_bad=loss_bad2)
            p, switch_rate, _ = phi(hop1, hop2)
            row_phi.append(p)
            row_mask.append(switch_rate < 0.01)  # degenerate: always-warm policy essentially never switches
        cells = []
        for p, degenerate in zip(row_phi, row_mask):
            marker = "(D)" if degenerate else "   "
            cells.append(f"{p:+.4f}{marker}".rjust(10))
        print(f"lambda2={l2:.2f}: " + "".join(cells))
    print("\n(D) marks a degenerate point (always-warm's own routing policy switches to B in <1%")
    print("of belief states -- i.e. the warm-vs-cold comparison is close to moot there since B is")
    print("almost never used regardless; exclude these from any 'clean structure' claim)\n")


if __name__ == "__main__":
    check1_mean_preserving_spread()
    check2_degenerate_boundary_overlap()
    check3_sign_map()
