"""Closed-form n_BB* (bump-free) threshold, via the SAME sign-collapse trick used for n_H*
(see WARM_COLD_PURE_GILBERT_NOTES.md, "n_BB* THRESHOLD -- SAME TRICK APPLIES"). P_GG_BB(n) =
(1-pi_b)^2*(1-lambda^n)^2 is monotone increasing (no bump), saturating at (1-pi_b)^2 -- using
that saturation value in place of the H-side's bump maximum gives
  cost_a*_BB = (1+2*c_switch_cold) / (1 + N*(1-pi_b)^2)
Verified here against a fine cost_a sweep of the standalone BB-only toy model (N_MAX=2000, well
away from the search boundary). Note: this standalone threshold is NOT identical to where n_BB*
leaves the boundary in the full coupled embedded chain (see notes' "coupling caveat") -- it is
exact only for the BB entry analyzed in isolation.

Run with: uv run --with numpy python pure_gilbert_nbb_threshold_bracket_check_demo.py
"""

import numpy as np

def cost_a_threshold_BB(pi_b, lam, c_switch_cold):
    p_gb = pi_b*(1-lam)
    q_G = 1-p_gb
    N = 1/(1-q_G**2)
    f_max = (1-pi_b)**2  # P_GG_BB saturation value as n->infinity
    return (1+2*c_switch_cold) / (1 + N*f_max)

pi_b, lam, c_sw = 0.8, 0.85, 0.05
thresh_bb = cost_a_threshold_BB(pi_b, lam, c_sw)
print("closed-form cost_a*_BB threshold =", thresh_bb)

p_gb = pi_b*(1-lam); q_G=1-p_gb; N=1/(1-q_G**2)
def g_toy_BB(n, cost_a):
    x = lam**n
    A = 1+2*c_sw-cost_a
    B = cost_a
    P_GG = (1-pi_b)**2*(1-x)**2
    return (A+B*n)/(n+N*P_GG)

ns = np.arange(2, 2001)
for ca in [thresh_bb-0.002, thresh_bb-0.0005, thresh_bb+0.0005, thresh_bb+0.002]:
    vals = g_toy_BB(ns, ca)
    idx = np.argmin(vals)
    print(f"cost_a={ca:.5f}: argmin n={ns[idx]}, g_min={vals[idx]:.6f}, beats plateau={vals[idx]<ca-1e-9}, at N_MAX boundary={ns[idx]==2000}")
