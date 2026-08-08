"""task2-gate (task #58): before attempting the heavier Lipschitz/span-
contraction sufficient-condition derivation (task #49(b)), run a cheap,
independent numerical check: at the paper's actual calibrated scenario
(`switching_curves_demo.py`'s hop1/hop2/cost_a/c_warm/c_switch_*), is there a
non-vacuous MARGIN between how fast the routing d-field actually rises and
how fast the value-of-information hump-term (the thing with no monotonicity
guarantee) could plausibly push it back down? If the margin is already
razor-thin or negative at the one scenario that matters most, the full
derivation is pointless and #49 should go straight to the fallback (a
numerically-certified policy class for the calibrated regime, not a general
theorem).

This is NOT a rigorous base/VoI-term decomposition of d_field_full_model (no
closed form for that split exists in the full 4-action model, unlike the
always-warm sub-model where `base(beta,a)` is literally isolable -- see
THRESHOLD_PROOF.md §3). It is a heuristic safety-factor proxy: compare the
SCALE of d's own finite-difference slope (how much positive slope the
routing decision boundary actually has -- the margin available before a
violation would appear) against the SCALE of the VoI-gap field's own slope
(`invariant_features_demo.py`'s `voi_gap`, an upper bound on how much a
hump-shaped confound COULD subtract from that slope). A large ratio means
the calibrated scenario is comfortably inside the monotone regime, not
riding right at the edge of it.

Run with: uv run python voi_margin_gate_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby
from invariant_features_demo import voi_gap

# The paper's calibrated/representative scenario (switching_curves_demo.py).
HOP1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
HOP2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
COST_A = 0.08
C_WARM, C_SWITCH_WARM, C_SWITCH_COLD = 0.06, 0.01, 0.5


def min_positive_slope(field_flat: np.ndarray, grid) -> tuple[float, float]:
    """Returns (min slope over beta1-direction pairs, min slope over
    beta2-direction pairs) -- the SMALLEST per-step increase anywhere on the
    grid. This is the routing d-field's margin: how far the closest call is
    from becoming an actual violation (a negative value here)."""
    field_grid = field_flat.reshape(grid.shape)
    diff1 = field_grid[1:, :] - field_grid[:-1, :]
    diff2 = field_grid[:, 1:] - field_grid[:, :-1]
    return float(diff1.min()), float(diff2.min())


def max_abs_slope(field_flat: np.ndarray, grid) -> tuple[float, float]:
    field_grid = field_flat.reshape(grid.shape)
    diff1 = field_grid[1:, :] - field_grid[:-1, :]
    diff2 = field_grid[:, 1:] - field_grid[:, :-1]
    return float(np.max(np.abs(diff1))), float(np.max(np.abs(diff2)))


def main() -> None:
    path_b_loss = channels.path_b_loss_prob(HOP1, HOP2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)

    resolution = 150
    print(f"=== Calibrated scenario, resolution={resolution} ===")
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(HOP1, HOP2, cost, resolution=resolution, n_iters=3000)

    worst_min_slope = float("inf")
    worst_voi_slope = 0.0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            min1, min2 = min_positive_slope(d_full, sol.grid)
            mono = switching_curves.check_monotone_grid(d_full, sol.grid)
            gap = voi_gap(sol.grid, HOP1, HOP2, sol.h[:, p, w])
            voi1, voi2 = max_abs_slope(gap, sol.grid)
            print(f"\ncontext (p={p}, w={w}):")
            print(f"  d-field min per-step slope: beta1-dir={min1:.4e}, beta2-dir={min2:.4e} "
                  f"(negative would BE a violation; {mono['n_violations_beta1']}+{mono['n_violations_beta2']} found)")
            print(f"  voi_gap field max |slope|:  beta1-dir={voi1:.4e}, beta2-dir={voi2:.4e}")
            worst_min_slope = min(worst_min_slope, min1, min2)
            worst_voi_slope = max(worst_voi_slope, voi1, voi2)

    print(f"\n=== Margin summary ===")
    print(f"worst-case (smallest) d-field slope across all contexts/directions: {worst_min_slope:.4e}")
    print(f"worst-case (largest) voi_gap-field slope across all contexts/directions: {worst_voi_slope:.4e}")
    ratio = worst_min_slope / worst_voi_slope if worst_voi_slope > 0 else float("inf")
    print(f"safety-factor ratio (d's own slope margin / voi_gap's slope scale): {ratio:.3f}")

    print("\n=== Verdict ===")
    if worst_min_slope <= 0:
        print("The calibrated scenario ALREADY has a monotonicity violation at this resolution --")
        print("#49 should go straight to the numerically-certified-policy-class fallback, no general")
        print("sufficient-condition derivation is worth attempting.")
    elif ratio < 1.0:
        print("d's own slope margin is SMALLER than the VoI-gap field's slope scale at the calibrated")
        print("scenario -- the margin is thin even where no violation currently occurs. A general")
        print("Lipschitz/span-contraction sufficient-condition derivation is unlikely to close this")
        print("gap cleanly; #49 should lean toward the numerically-certified fallback rather than")
        print("investing in the full derivation.")
    else:
        print(f"d's own slope margin is {ratio:.1f}x LARGER than the VoI-gap field's slope scale at the")
        print("calibrated scenario -- there is real headroom here, not a razor's edge. This is a")
        print("positive (though heuristic, not rigorous) signal that a sufficient-condition proof")
        print("bounding the VoI term's Lipschitz constant against the base term's guaranteed slope")
        print("could plausibly close a real gap for #49(b), rather than chasing a vacuously-true or")
        print("already-false claim.")


if __name__ == "__main__":
    main()
