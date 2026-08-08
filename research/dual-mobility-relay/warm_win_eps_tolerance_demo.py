"""For representative (pi_b, lambda) points, finds how far eps_good/eps_bad can depart from the
pure-Gilbert idealization (0,1) before the warm-win window (see
warm_win_phase_diagram_pure_gilbert_demo.py) closes entirely -- i.e. the "blur tolerance" of the
warm-win phase diagram, using the GENERAL (eps-aware) switching_curves solver.

For each test point, holds (p_gb, p_bg) fixed (so pi_b, lambda stay fixed) and interpolates
eps_good from 0 toward a target value, eps_bad from 1 toward a target value, using t in [0,1] as
the interpolation fraction. Finds the largest t (finest resolution feasible) for which a warm-win
window (Phi<0 somewhere) still exists, over a cost_a search anchored near the pure-Gilbert
closed-form lower edge.

Run with: uv run python warm_win_eps_tolerance_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

RESOLUTION = 60
N_ITERS = 2000


def cost_a_lo_closed_form(pi_b: float, lam: float, c_warm: float, c_switch_warm: float) -> float:
    p_gb = pi_b * (1 - lam)
    q_g = 1 - p_gb
    return c_warm / (1 - pi_b) ** 2 + (1 - q_g ** 2) * (1 + 2 * c_switch_warm)


def phi(cost_a: float, p_gb: float, p_bg: float, eps_good: float, eps_bad: float,
        c_warm: float, c_switch_warm: float, c_switch_cold: float) -> float:
    hop = channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=eps_good, eps_bad=eps_bad)
    sol_warm = switching_curves.always_warm_value_iteration(
        hop, hop, cost_a, c_warm, c_switch_warm, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(
        hop, hop, cost_a, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g - sol_cold.g


def window_exists(p_gb, p_bg, eps_good, eps_bad, c_warm, c_switch_warm, c_switch_cold,
                   cost_a_lo_approx) -> tuple[bool, float]:
    grid = np.concatenate([
        np.linspace(0.5 * cost_a_lo_approx, cost_a_lo_approx, 4),
        np.linspace(cost_a_lo_approx, 2.5 * cost_a_lo_approx, 10),
    ])
    min_phi = float("inf")
    for ca in grid:
        p = phi(float(ca), p_gb, p_bg, eps_good, eps_bad, c_warm, c_switch_warm, c_switch_cold)
        min_phi = min(min_phi, p)
    return min_phi < -1e-9, min_phi


def find_tolerance(pi_b, lam, target_eps_good, target_eps_bad, c_warm, c_switch_warm, c_switch_cold):
    p_gb = pi_b * (1 - lam)
    p_bg = (1 - pi_b) * (1 - lam)
    cost_a_lo_approx = cost_a_lo_closed_form(pi_b, lam, c_warm, c_switch_warm)

    ts = np.linspace(0.0, 1.0, 11)
    results = []
    for t in ts:
        eg = 0.0 + t * target_eps_good
        eb = 1.0 + t * (target_eps_bad - 1.0)
        exists, min_phi = window_exists(p_gb, p_bg, eg, eb, c_warm, c_switch_warm, c_switch_cold,
                                          cost_a_lo_approx)
        results.append((t, eg, eb, exists, min_phi))
    return results


def main() -> None:
    C_WARM = 0.02
    C_SWITCH_WARM = 0.01
    C_SWITCH_COLD = 0.02

    # representative points, interpolating toward "moderately realistic" eps targets
    # (roughly Berlin-V2X scale: eps_bad down to ~0.3-0.5, eps_good up to ~0.05-0.1)
    test_points = [
        (0.1, 0.4, 0.05, 0.40),
        (0.3, 0.4, 0.05, 0.40),
        (0.5, 0.4, 0.05, 0.40),
        (0.3, 0.2, 0.05, 0.40),
        (0.3, 0.8, 0.05, 0.40),
    ]

    for pi_b, lam, teg, teb in test_points:
        print(f"=== pi_b={pi_b}, lambda={lam}, target eps=({teg},{teb}) ===")
        results = find_tolerance(pi_b, lam, teg, teb, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)
        for t, eg, eb, exists, min_phi in results:
            print(f"  t={t:.1f} eps=({eg:.4f},{eb:.4f}): window_exists={exists}, min_Phi={min_phi:+.6f}")
        # find the last t where window still exists
        existing_ts = [t for t, _, _, exists, _ in results if exists]
        if existing_ts:
            print(f"  -> tolerates up to t={max(existing_ts):.1f} of the way toward the target eps\n")
        else:
            print(f"  -> window does not survive even the smallest tested eps departure\n")


if __name__ == "__main__":
    main()
