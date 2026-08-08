"""Numeric bracketing check for the closed-form cost_a* threshold derived in
`pure_gilbert_nh_threshold_derivation_demo.py` (see WARM_COLD_PURE_GILBERT_NOTES.md,
"CLOSED-FORM n_H* BUMP-TRANSITION THRESHOLD"). Sweeps cost_a on a fine grid straddling the
predicted threshold and confirms "does the argmin beat the plateau (cost_a itself)" flips sign
exactly at the predicted value, for pi_b=0.8, lambda=0.85, c_switch_cold=0.05 (cost_a*=0.86137).

Run with: uv run --with numpy python pure_gilbert_nh_threshold_bracket_check_demo.py
"""

import numpy as np

def cost_a_threshold(pi_b, lam, c_switch_cold):
    p_gb = pi_b*(1-lam)
    q_G = 1-p_gb
    N = 1/(1-q_G**2)
    return (1+2*c_switch_cold) / (1 + N*(1-pi_b)/(4*pi_b))

pi_b, lam, c_sw = 0.8, 0.85, 0.05
thresh = cost_a_threshold(pi_b, lam, c_sw)
print("closed-form cost_a* threshold =", thresh)

# fine grid near threshold, full toy model check via g(n)-cost_a sign
p_gb = pi_b*(1-lam); q_G=1-p_gb; N=1/(1-q_G**2)
def g_toy(n, cost_a):
    x = lam**n
    A = 1+2*c_sw-cost_a
    B = cost_a
    P_GG = (1-pi_b)*(1-x)*(1-pi_b+pi_b*x)
    return (A+B*n)/(n+N*P_GG)

ns = np.arange(2, 201)
for ca in [thresh-0.002, thresh-0.0005, thresh+0.0005, thresh+0.002]:
    vals = g_toy(ns, ca)
    idx = np.argmin(vals)
    print(f"cost_a={ca:.5f}: argmin n={ns[idx]}, g_min={vals[idx]:.6f}, cost_a={ca:.6f}, beats plateau={vals[idx]<ca-1e-9}")
