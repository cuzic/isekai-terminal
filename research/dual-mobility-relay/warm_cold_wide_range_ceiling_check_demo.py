"""Extends the "shallow valley hugs the c_warm ceiling" finding (found via
pure_gilbert_closed_form_as_approximation_v2_demo.py + opus-symbolic-advisor's diagnosis,
2026-07-19) to (a) hop2's own real parameters, and (b) the ACTUAL asymmetric real pair
(hop1+hop2 together, matching THRESHOLD_PROOF.md section 6's exact setup) -- across a WIDE
cost_a range, not just near the previously-found narrow window -- to test the stronger claim
the advisor proposed: "in the real eps regime, cold weakly dominates almost everywhere (Phi stays
pinned near +c_warm across nearly the full cost_a range), not just narrowly near a boundary."

This produces the data for the paper's key figure: Phi(cost_a) hugging the PROVEN c_warm ceiling
(Phi<=c_warm, THRESHOLD_PROOF.md section 4), with the narrow dip (where it exists at all) being
the only place adaptive/warm complexity could ever pay off.

Run with: uv run python warm_cold_wide_range_ceiling_check_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

C_WARM = 0.005
C_SWITCH_WARM = 0.01
C_SWITCH_COLD = 0.02
RESOLUTION = 60
N_ITERS = 2000

HOP1 = channels.HopParams(p_gb=0.1909, p_bg=0.4553, eps_good=0.0320, eps_bad=0.3010)
HOP2 = channels.HopParams(p_gb=0.2764, p_bg=0.3933, eps_good=0.0695, eps_bad=0.4253)


def phi(cost_a: float, hop1: channels.HopParams, hop2: channels.HopParams) -> float:
    sol_warm = switching_curves.always_warm_value_iteration(
        hop1, hop2, cost_a, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(
        hop1, hop2, cost_a, C_SWITCH_COLD, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g - sol_cold.g


def main() -> None:
    cost_a_grid = np.concatenate([
        np.linspace(0.02, 0.25, 8),
        np.linspace(0.27, 0.34, 15),  # fine near the known real dip
        np.linspace(0.36, 0.60, 7),
    ])

    print("=== hop2-only (symmetric, hop2=hop2), wide cost_a range ===")
    print(f"{'cost_a':>8} {'Phi':>10} {'Phi/c_warm':>11}")
    for ca in cost_a_grid:
        p = phi(float(ca), HOP2, HOP2)
        print(f"{ca:>8.3f} {p:>10.6f} {p / C_WARM:>10.3f}")

    print("\n=== ACTUAL asymmetric real pair (hop1 + hop2, matches THRESHOLD_PROOF.md sec.6) ===")
    print(f"{'cost_a':>8} {'Phi':>10} {'Phi/c_warm':>11}")
    phis = []
    for ca in cost_a_grid:
        p = phi(float(ca), HOP1, HOP2)
        phis.append(p)
        print(f"{ca:>8.3f} {p:>10.6f} {p / C_WARM:>10.3f}")

    phis = np.array(phis)
    frac_near_ceiling = np.mean(phis > 0.9 * C_WARM)
    print(f"\nFraction of the swept cost_a range where Phi > 90% of the c_warm ceiling: "
          f"{frac_near_ceiling*100:.1f}%")
    print(f"Minimum Phi observed anywhere in this wide sweep: {phis.min():.6f} "
          f"({phis.min()/C_WARM*100:.1f}% of c_warm), at cost_a={cost_a_grid[np.argmin(phis)]:.3f}")


if __name__ == "__main__":
    main()
