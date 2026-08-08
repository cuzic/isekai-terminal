"""Per opus-symbolic-advisor's recommendation (2026-07-19): before symbolically refining the
pure-Gilbert Phi=0 active-cell validity domain, locate the REAL calibrated operating point
relative to a Phi=0-style boundary using the project's GENERAL (asymmetric, partial-observation,
eps NOT restricted to 0/1) solver -- since the pure-Gilbert idealization's eps assumption doesn't
literally hold for real channels (hop1/hop2 fitted eps_good~0.03-0.07, eps_bad~0.30-0.43, and
hop1!=hop2), the pure-Gilbert Phi=0 closed form is an analytical ANCHOR/limiting case, not a
literal predictor -- the real answer must come from `switching_curves.always_warm_value_iteration`
/ `always_cold_value_iteration`, which already handle general asymmetric partial-observation
channels directly.

Uses the real Berlin V2X EM-fitted hop parameters (from `berlin_v2x_block_fit_demo.py`):
  hop1 (car4->car2): p_gb=0.1909, p_bg=0.4553, eps_good=0.0320, eps_bad=0.3010
  hop2 (car3->car1): p_gb=0.2764, p_bg=0.3933, eps_good=0.0695, eps_bad=0.4253

Sweeps cost_a (holding c_warm, c_switch_warm fixed at this project's calibrated values) to find
where g_warm*(fixed always-warm policy) = g_cold*(fixed always-cold policy) crosses -- i.e. the
REAL, asymmetric, partial-observation analogue of Phi=0 -- and reports where the actual
calibrated cost_a=0.30 sits relative to that crossing, plus how flat the gap is nearby (connects
to task #5, THRESHOLD_PROOF.md/paper: explains analytically why the real adaptive-gain finding
was small).

Run with: uv run python warm_cold_phi_zero_real_data_position_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

C_WARM = 0.005
C_SWITCH_WARM = 0.01
C_SWITCH_COLD = 0.02  # matches this project's peak-gain operating point (TRACE_CALIBRATION_NOTES.md:
                       # "peak relative value 0.65% at c_warm=0.005, c_switch_cold=0.02")
RESOLUTION = 60
N_ITERS = 2000

HOP1 = channels.HopParams(p_gb=0.1909, p_bg=0.4553, eps_good=0.0320, eps_bad=0.3010)
HOP2 = channels.HopParams(p_gb=0.2764, p_bg=0.3933, eps_good=0.0695, eps_bad=0.4253)

REAL_COST_A = 0.30  # this project's calibrated value (see TRACE_CALIBRATION_NOTES.md)


def phi(cost_a: float) -> tuple[float, float, float]:
    sol_warm = switching_curves.always_warm_value_iteration(
        HOP1, HOP2, cost_a, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(
        HOP1, HOP2, cost_a, C_SWITCH_COLD, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g, sol_cold.g, sol_warm.g - sol_cold.g


def main() -> None:
    print("=== Real (asymmetric, partial-obs) Berlin V2X Phi sweep ===")
    print(f"hop1: p_gb={HOP1.p_gb}, p_bg={HOP1.p_bg}, eps_good={HOP1.eps_good}, eps_bad={HOP1.eps_bad}")
    print(f"hop2: p_gb={HOP2.p_gb}, p_bg={HOP2.p_bg}, eps_good={HOP2.eps_good}, eps_bad={HOP2.eps_bad}")
    print(f"c_warm={C_WARM}, c_switch_warm={C_SWITCH_WARM}, c_switch_cold={C_SWITCH_COLD}\n")

    stationary1 = HOP1.p_gb / (HOP1.p_gb + HOP1.p_bg)
    stationary2 = HOP2.p_gb / (HOP2.p_gb + HOP2.p_bg)
    print(f"hop1 pi_b={stationary1:.4f}, hop2 pi_b={stationary2:.4f}\n")

    print(f"{'cost_a':>8}  {'g_warm*':>10}  {'g_cold*':>10}  {'Phi=warm-cold':>14}")
    cost_a_grid = np.linspace(0.05, 0.60, 23)
    results = []
    for ca in cost_a_grid:
        gw, gc, ph = phi(float(ca))
        results.append((ca, gw, gc, ph))
        print(f"{ca:>8.4f}  {gw:>10.6f}  {gc:>10.6f}  {ph:>+14.6f}")

    # Locate sign change (Phi=0 crossing), if any, by bracketing
    print()
    crossed = False
    for (ca1, _, _, p1), (ca2, _, _, p2) in zip(results, results[1:]):
        if p1 == 0 or (p1 < 0) != (p2 < 0):
            crossed = True
            print(f"Phi=0 crossing bracketed between cost_a={ca1:.4f} (Phi={p1:+.6f}) and "
                  f"cost_a={ca2:.4f} (Phi={p2:+.6f})")
    if not crossed:
        print("NO sign change detected in the swept range -- one side dominates throughout "
              f"[{cost_a_grid[0]:.2f}, {cost_a_grid[-1]:.2f}].")

    print(f"\nAt the REAL calibrated cost_a={REAL_COST_A}:")
    gw, gc, ph = phi(REAL_COST_A)
    print(f"  g_warm*={gw:.6f}, g_cold*={gc:.6f}, Phi={ph:+.6f} "
          f"({'warm-fixed worse (cold wins)' if ph > 0 else 'cold-fixed worse (warm wins)'})")
    print(f"  |Phi| relative to cost_a: {abs(ph)/REAL_COST_A*100:.3f}%")


if __name__ == "__main__":
    main()
