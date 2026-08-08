"""Corrected version of warm_win_eps_tolerance_demo.py: the first attempt centered its cost_a
search grid on the naive pure-Gilbert CLOSED-FORM estimate of cost_a_lo, which
warm_win_c_warm_scaling_demo.py already showed can be far from the TRUE crossing once outside the
closed form's domain of validity (e.g. pi_b=0.1,lambda=0.4's true window is [0.688,0.861] per
warm_win_phase_diagram_pure_gilbert_demo.py, but the closed-form estimate was only ~0.143 --
completely missing the true window). This produced a false "no window" verdict at t=0
(exact pure-Gilbert) for that point, which should be impossible (t=0 must reduce to the already-
verified phase diagram). Fixed here by using the ACTUAL known window bounds [cost_a_lo,
cost_a_hi] from the phase diagram (passed in explicitly per point) as the search range basis,
not a fixed multiple of the closed-form estimate.

Run with: uv run python warm_win_eps_tolerance_v2_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

RESOLUTION = 60
N_ITERS = 2000


def phi(cost_a: float, p_gb: float, p_bg: float, eps_good: float, eps_bad: float,
        c_warm: float, c_switch_warm: float, c_switch_cold: float) -> float:
    hop = channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=eps_good, eps_bad=eps_bad)
    sol_warm = switching_curves.always_warm_value_iteration(
        hop, hop, cost_a, c_warm, c_switch_warm, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(
        hop, hop, cost_a, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g - sol_cold.g


def window_exists(p_gb, p_bg, eps_good, eps_bad, c_warm, c_switch_warm, c_switch_cold,
                   known_lo, known_hi) -> tuple[bool, float]:
    # search a range comfortably wider than the KNOWN true pure-Gilbert window, not a
    # closed-form-centered guess -- avoids the search-range-miss artifact found in v1.
    grid = np.linspace(0.7 * known_lo, 1.3 * known_hi, 25)
    min_phi = float("inf")
    for ca in grid:
        p = phi(float(ca), p_gb, p_bg, eps_good, eps_bad, c_warm, c_switch_warm, c_switch_cold)
        min_phi = min(min_phi, p)
    return min_phi < -1e-9, min_phi


def find_tolerance(pi_b, lam, known_lo, known_hi, target_eps_good, target_eps_bad,
                    c_warm, c_switch_warm, c_switch_cold):
    p_gb = pi_b * (1 - lam)
    p_bg = (1 - pi_b) * (1 - lam)
    ts = np.linspace(0.0, 1.0, 11)
    results = []
    for t in ts:
        eg = 0.0 + t * target_eps_good
        eb = 1.0 + t * (target_eps_bad - 1.0)
        exists, min_phi = window_exists(p_gb, p_bg, eg, eb, c_warm, c_switch_warm, c_switch_cold,
                                          known_lo, known_hi)
        results.append((t, eg, eb, exists, min_phi))
    return results


def main() -> None:
    C_WARM = 0.02
    C_SWITCH_WARM = 0.01
    C_SWITCH_COLD = 0.02

    # (pi_b, lambda, known_cost_a_lo, known_cost_a_hi) from
    # warm_win_phase_diagram_pure_gilbert_demo.py's own exact output -- re-testing exactly the
    # two points v1 got wrong/borderline (pi_b=0.1,lambda=0.4 and pi_b=0.3,lambda=0.2), plus one
    # already-reliable point as a sanity re-check (pi_b=0.3,lambda=0.4).
    test_points = [
        (0.1, 0.4, 0.68841, 0.86052, 0.05, 0.40),
        (0.3, 0.2, 0.52131, 2.82999, 0.05, 0.40),
        (0.3, 0.4, 0.41444, 2.24981, 0.05, 0.40),
    ]

    for pi_b, lam, lo, hi, teg, teb in test_points:
        print(f"=== pi_b={pi_b}, lambda={lam}, known pure-Gilbert window=[{lo:.3f},{hi:.3f}], "
              f"target eps=({teg},{teb}) ===")
        results = find_tolerance(pi_b, lam, lo, hi, teg, teb, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)
        for t, eg, eb, exists, min_phi in results:
            print(f"  t={t:.1f} eps=({eg:.4f},{eb:.4f}): window_exists={exists}, min_Phi={min_phi:+.6f}")
        existing_ts = [t for t, _, _, exists, _ in results if exists]
        if existing_ts:
            print(f"  -> tolerates up to t={max(existing_ts):.1f} of the way toward the target eps\n")
        else:
            print(f"  -> window does not survive even the smallest tested eps departure\n")


if __name__ == "__main__":
    main()
