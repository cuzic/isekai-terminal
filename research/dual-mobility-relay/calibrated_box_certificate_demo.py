"""Task #65 (A-fallback): originally attempted as a "certified numerical
theorem for a declared parameter box" (Codex's suggested 4th direction,
2026-07-18 consultation). AN INDEPENDENT CODEX REVIEW OF THIS SPECIFIC
SCRIPT (2026-07-18) FOUND THE "CERTIFICATE" CLAIM WAS MATHEMATICALLY WRONG,
not just imprecisely worded -- corrected below to an honest, much weaker
"empirical margin diagnostic." Four real problems, not style nits:

1. `M2*h^2/8` bounds how far a LINEAR INTERPOLANT's VALUE can be from the
   true continuous function between two grid points (the standard Taylor-
   remainder interpolation-error bound). That is NOT the right quantity for
   a MONOTONICITY claim -- monotonicity needs a bound on how much the
   DERIVATIVE can vary, which is a different (looser, by roughly a factor
   of 4) inequality (`endpoint_diff > M2*h^2/2` in these units, not `/8`).
2. What this script calls `min_slope` is actually the raw grid-step
   DIFFERENCE (`field[i+1]-field[i]`), not divided by `h` -- mislabeled as
   a slope throughout, including in the original THRESHOLD_PROOF.md writeup.
3. The check only examines second differences ALONG grid lines (fixed
   beta2 varying beta1, and vice versa). It says nothing about monotonicity
   at off-grid-line points or about mixed partial derivatives -- a genuine
   coverage gap, not just a wording issue.
4. `d_field_full_model` is built from `min()` over action subsets (see that
   function's own docstring) -- at any belief where the argmin action
   switches, `d` can have a KINK (a point where it is continuous but not
   twice differentiable). A finite, uniform second-derivative bound `M2`
   might not even EXIST globally in that case, and computing it via finite
   differences on ANY grid (including the "fine" resolution=300 grid used
   here, which is only 2x finer than resolution=150 -- nowhere near the
   "much finer"/"order of magnitude" scale this kind of empirical bound
   would need) can silently underestimate it by missing or smoothing over
   a kink between sample points.

None of this means the calibrated scenario's monotonicity is in doubt --
independent checks elsewhere (`check_monotone_grid` at resolution up to
150, the direct zero-violation results already in THRESHOLD_PROOF.md) still
stand. It means THIS SPECIFIC SCRIPT does not add a rigorous "certificate"
on top of those -- it is a diagnostic showing the raw per-step differences
are comfortably larger than a plausible-looking (but not soundly derived)
error term, nothing more. Kept here, relabeled honestly, because the raw
numbers are still a mildly informative cross-check, not because the
original "certified" framing was salvageable.

Run with: uv run python calibrated_box_certificate_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

# The paper's calibrated/representative scenario (switching_curves_demo.py).
HOP1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
HOP2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
COST_A = 0.08
C_WARM, C_SWITCH_WARM, C_SWITCH_COLD = 0.06, 0.01, 0.5


def solve(resolution: int, n_iters: int = 3000):
    path_b_loss = channels.path_b_loss_prob(HOP1, HOP2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)
    return beliefgrid2d.belief_grid2d_value_iteration_warm(HOP1, HOP2, cost, resolution=resolution, n_iters=n_iters)


def second_diff_over_h2(field_grid: np.ndarray, h: float, axis: int) -> float:
    """Max |second finite difference| / h^2 along the given axis (0=beta1, 1=beta2) --
    an empirical, NOT rigorously bounding, estimate of local curvature. See this
    module's docstring: `d_field_full_model` involves `min()` over action subsets and
    can have kinks at action-switching beliefs, so a genuine uniform second-derivative
    bound may not exist at all -- this number is a diagnostic magnitude, not M2 in the
    rigorous Taylor-remainder sense."""
    if axis == 0:
        d2 = field_grid[2:, :] - 2 * field_grid[1:-1, :] + field_grid[:-2, :]
    else:
        d2 = field_grid[:, 2:] - 2 * field_grid[:, 1:-1] + field_grid[:, :-2]
    return float(np.max(np.abs(d2)) / h ** 2)


def min_step_diff(field_grid: np.ndarray, axis: int) -> float:
    """Minimum RAW grid-step difference (field[i+1]-field[i]), NOT divided by h --
    i.e. NOT a slope/derivative, despite loosely resembling one. Named precisely per
    Codex review (2026-07-18), which caught the original version calling this
    `min_slope` while never dividing by h."""
    diffs = field_grid[1:, :] - field_grid[:-1, :] if axis == 0 else field_grid[:, 1:] - field_grid[:, :-1]
    return float(diffs.min())


def main() -> None:
    print("=== Empirical curvature MAGNITUDE (not a rigorous bound), from a resolution=300 grid ===")
    sol_fine = solve(300, n_iters=4000)
    curv_beta1 = curv_beta2 = 0.0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol_fine, p, w)
            field_grid = d_full.reshape(sol_fine.grid.shape)
            h_fine = 1.0 / 300
            curv_beta1 = max(curv_beta1, second_diff_over_h2(field_grid, h_fine, axis=0))
            curv_beta2 = max(curv_beta2, second_diff_over_h2(field_grid, h_fine, axis=1))
    print(f"curvature magnitude (beta1-direction) = {curv_beta1:.4e}")
    print(f"curvature magnitude (beta2-direction) = {curv_beta2:.4e}")
    print("(resolution=300 is only 2x resolution=150's grid spacing -- NOT an order-of-magnitude")
    print(" finer estimate, and d_field_full_model's min()-induced kinks mean no single finite")
    print(" resolution can reliably rule out underestimating this number; see module docstring)")

    print("\n=== Diagnostic: does the observed grid-step difference dwarf a naive (unsound) error term? ===")
    print("(this compares against M2*h^2/8, the LINEAR-INTERPOLATION VALUE error bound, which Codex's")
    print(" review confirmed is NOT the correct quantity for a monotonicity argument -- treat this as")
    print(" a loose magnitude comparison only, not a pass/fail proof gate)")
    for resolution in [30, 60, 100, 150]:
        sol = solve(resolution, n_iters=3000)
        h = 1.0 / resolution
        naive_term_1 = curv_beta1 * h ** 2 / 8
        naive_term_2 = curv_beta2 * h ** 2 / 8
        worst_diff_1 = float("inf")
        worst_diff_2 = float("inf")
        for p in range(2):
            for w in range(2):
                d_full = switching_curves.d_field_full_model(sol, p, w)
                field_grid = d_full.reshape(sol.grid.shape)
                worst_diff_1 = min(worst_diff_1, min_step_diff(field_grid, axis=0))
                worst_diff_2 = min(worst_diff_2, min_step_diff(field_grid, axis=1))

        cmp_1 = "step-diff LARGER" if worst_diff_1 > naive_term_1 else "step-diff smaller"
        cmp_2 = "step-diff LARGER" if worst_diff_2 > naive_term_2 else "step-diff smaller"
        print(f"resolution={resolution:>3}: beta1-dir step_diff={worst_diff_1:.4e} vs "
              f"naive_term={naive_term_1:.4e} [{cmp_1}], "
              f"beta2-dir step_diff={worst_diff_2:.4e} vs naive_term={naive_term_2:.4e} [{cmp_2}]")

    print("\n=== Verdict ===")
    print("This script does NOT produce a certificate or proof of monotonicity, at any resolution.")
    print("An independent Codex review (2026-07-18) found the original 'certificate' framing rested")
    print("on the wrong error bound (an interpolation-VALUE bound, not a monotonicity/derivative")
    print("bound), missed off-grid-line and mixed-partial coverage, and used a 'fine' grid only 2x")
    print("finer than the coarsest resolution being checked -- while d_field_full_model's min()-based")
    print("construction may not even have a uniform second-derivative bound to estimate in the first")
    print("place. The numbers above are a loose magnitude cross-check, not a proof gate: the raw grid-")
    print("step differences are comfortably larger than this naive (unsound) error term at every")
    print("resolution from 60 up, consistent with (but not adding rigor beyond) the zero-violation")
    print("results already established elsewhere in this project via direct check_monotone_grid calls.")
    print("A real interval-arithmetic or validated-numerics treatment of this question, handling the")
    print("min()-induced kinks explicitly, remains open future work -- not completed here.")


if __name__ == "__main__":
    main()
