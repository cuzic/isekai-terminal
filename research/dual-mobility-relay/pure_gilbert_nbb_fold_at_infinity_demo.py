"""Confirms the advisor's structural point: unlike n_H (bump, finite vertex, clean algebraic
tangency at detachment), n_BB's P_GG_BB(n)=(1-pi_b)^2*(1-lambda^n)^2 is strictly monotone with NO
finite vertex, so detachment from the plateau is a transcendental "fold at infinity" -- as cost_a
approaches cost_a*_BB from above, the argmin location grows without bound rather than jumping to
a fixed finite value. Confirmed here: argmin n at cost_a*_BB+delta grows 20->27->32->...->69 as
delta shrinks from 0.05 to 0.00005, never settling (see WARM_COLD_PURE_GILBERT_NOTES.md,
"CORRECTION: the two-stage full-embedded-chain finding was itself a resolution artifact").

Run with: uv run --with numpy python pure_gilbert_nbb_fold_at_infinity_demo.py
"""

import numpy as np

pi_b, lam, c_sw = 0.8, 0.85, 0.05
p_gb = pi_b*(1-lam); q_G=1-p_gb; N=1/(1-q_G**2)

def g_toy_BB(n, cost_a):
    x = lam**n
    A = 1+2*c_sw-cost_a
    B = cost_a
    P_GG = (1-pi_b)**2*(1-x)**2
    return (A+B*n)/(n+N*P_GG)

def cost_a_threshold_BB(pi_b, lam, c_switch_cold):
    p_gb = pi_b*(1-lam); q_G=1-p_gb; N=1/(1-q_G**2)
    f_max = (1-pi_b)**2
    return (1+2*c_switch_cold) / (1 + N*f_max)

thresh = cost_a_threshold_BB(pi_b, lam, c_sw)
print("closed-form asymptotic threshold =", thresh)
print()

# Sweep cost_a approaching threshold from above, with a HUGE n range, track argmin n and whether
# it diverges (fold-at-infinity) as cost_a -> thresh+
ns = np.arange(2, 200001)
for delta in [0.05, 0.02, 0.01, 0.005, 0.002, 0.001, 0.0005, 0.0002, 0.0001, 0.00005]:
    ca = thresh + delta
    vals = g_toy_BB(ns, ca)
    idx = np.argmin(vals)
    n_star = ns[idx]
    print(f"cost_a=thresh+{delta:<9.5f} ({ca:.6f}): argmin n={n_star:>7d}, g_min={vals[idx]:.8f}, "
          f"at 200000 boundary={n_star==200000}")

print()
# Also re-check the FULL embedded chain at cost_a=0.88 with N_MAX extended to 2000 (advisor's specific ask)
