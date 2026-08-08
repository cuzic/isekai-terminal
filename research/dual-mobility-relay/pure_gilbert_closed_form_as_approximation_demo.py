"""Tests an engineering-value question raised by the user (2026-07-19): rather than only having
an EXACT closed form for the idealized pure-Gilbert case (eps_good=0, eps_bad=1), how well does
that SAME closed form work as a cheap, no-solver-required APPROXIMATION for the general
(partial-observation, eps in (0,1)) case -- i.e. distill the exact result into a simple formula
and measure its error, rather than only claiming exactness in the idealized corner.

Approach: fix (pi_b, lambda, c_warm, c_switch_warm, c_switch_cold) for a SYMMETRIC hop pair
(hop1=hop2, matching the closed form's own assumption). The closed form
  cost_a*_approx = c_warm/(1-pi_b)^2 + (1-q_G^2)*(1+2*c_switch_warm)
depends ONLY on (pi_b, lambda, c_warm, c_switch_warm) -- NOT on eps_good/eps_bad at all. Sweep
eps_good away from 0 and eps_bad away from 1 (i.e. move away from the pure-Gilbert idealization
toward more realistic partial-observation channels), and for each (eps_good, eps_bad) pair,
bisect the TRUE crossing cost_a*_true using the general switching_curves solver
(always_warm_value_iteration / always_cold_value_iteration, which handle arbitrary eps). Compare.

Run with: uv run python pure_gilbert_closed_form_as_approximation_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

PI_B = 0.3
LAM = 0.4
C_WARM = 0.02
C_SWITCH_WARM = 0.01
RESOLUTION = 60
N_ITERS = 2000

P_GB = PI_B * (1 - LAM)
P_BG = (1 - PI_B) * (1 - LAM)
Q_G = 1 - P_GB


def cost_a_star_pure_gilbert_closed_form() -> float:
    """The pure-Gilbert closed form -- notably, does NOT depend on eps_good/eps_bad at all."""
    return C_WARM / (1 - PI_B) ** 2 + (1 - Q_G ** 2) * (1 + 2 * C_SWITCH_WARM)


def phi(cost_a: float, eps_good: float, eps_bad: float) -> float:
    hop = channels.HopParams(p_gb=P_GB, p_bg=P_BG, eps_good=eps_good, eps_bad=eps_bad)
    # symmetric: hop1 = hop2. Use a nominal, small c_switch_cold just to define g_cold's own
    # policy space -- the crossing point itself is governed by g_warm vs the always-A-cold
    # plateau (g_cold=cost_a) in this regime, consistent with the closed-form's own active cell.
    sol_warm = switching_curves.always_warm_value_iteration(
        hop, hop, cost_a, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(
        hop, hop, cost_a, 0.05, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g - sol_cold.g


def bisect_cost_a_star(eps_good: float, eps_bad: float, lo: float = 0.05, hi: float = 0.5,
                        tol: float = 1e-4) -> float:
    f_lo = phi(lo, eps_good, eps_bad)
    # widen hi upward (staying below the known second-crossing region) until a sign change
    # brackets the FIRST crossing, rather than assuming a fixed hi=0.5 always suffices.
    f_hi = phi(hi, eps_good, eps_bad)
    while f_lo * f_hi > 0 and hi < 0.85:
        hi += 0.05
        f_hi = phi(hi, eps_good, eps_bad)
    if f_lo * f_hi > 0:
        return float("nan")  # no sign change found even after widening
    for _ in range(40):
        mid = (lo + hi) / 2
        f_mid = phi(mid, eps_good, eps_bad)
        if abs(f_mid) < tol or (hi - lo) < 1e-5:
            return mid
        if (f_mid > 0) == (f_lo > 0):
            lo, f_lo = mid, f_mid
        else:
            hi = mid
    return (lo + hi) / 2


def main() -> None:
    approx = cost_a_star_pure_gilbert_closed_form()
    print(f"pi_b={PI_B}, lambda={LAM}, c_warm={C_WARM}, c_switch_warm={C_SWITCH_WARM}")
    print(f"Pure-Gilbert closed-form cost_a*_approx = {approx:.5f} "
          f"(does NOT depend on eps_good/eps_bad)\n")

    print("Moving away from pure-Gilbert (eps_good=0, eps_bad=1) toward general partial "
          "observation:\n")
    print(f"{'eps_good':>9} {'eps_bad':>9} {'cost_a*_true':>13} {'cost_a*_approx':>15} "
          f"{'rel.err%':>9}")

    test_points = [
        (0.0, 1.0),      # exact pure-Gilbert (sanity check, should match closed form exactly)
        (0.01, 0.95),
        (0.02, 0.90),
        (0.05, 0.80),
        (0.05, 0.70),
        (0.10, 0.60),
        # realistic scale, matching Berlin V2X calibrated eps (hop1: 0.032/0.301, hop2: 0.0695/0.4253)
        (0.032, 0.301),
        (0.0695, 0.4253),
        (0.10, 0.50),
    ]

    for eps_good, eps_bad in test_points:
        true_val = bisect_cost_a_star(eps_good, eps_bad)
        rel_err = abs(true_val - approx) / true_val * 100 if not np.isnan(true_val) else float("nan")
        print(f"{eps_good:>9.3f} {eps_bad:>9.3f} {true_val:>13.5f} {approx:>15.5f} {rel_err:>8.2f}%")


if __name__ == "__main__":
    main()
