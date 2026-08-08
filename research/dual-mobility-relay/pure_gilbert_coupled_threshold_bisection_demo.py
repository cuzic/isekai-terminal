"""Rebuilds the full 2-state embedded g_cold(n_H,n_BB) and bisects the TRUE joint detachment
threshold (2D grid search, n_BB range pushed to log-spaced points up to 5,000,000 to escape the
"looks converged at N_MAX=200/2000" resolution trap -- see WARM_COLD_PURE_GILBERT_NOTES.md,
"CORRECTION: the two-stage full-embedded-chain finding was itself a resolution artifact"). Finds
the joint system detaches from the plateau at cost_a~0.8996 (pi_b=0.8, lambda=0.85,
c_switch_cold=0.05) -- clearly below the standalone BB-only threshold cost_a*_BB=0.9343, i.e. a
genuine coupling-driven downward shift (~0.0347), NOT the ~0.86 "n_H detaches first" claim an
earlier, under-resolved (N_MAX=200) sweep suggested.

Run with: uv run --with sympy --with numpy python pure_gilbert_coupled_threshold_bisection_demo.py
"""

import numpy as np
import sympy as sp

pi_b, lam, cost_a, c_switch_cold = sp.symbols("pi_b lambda cost_a c_switch_cold", positive=True)
n_H, n_BB = sp.symbols("n_H n_BB", positive=True, integer=True)
x_H, x_BB = sp.symbols("x_H x_BB", positive=True)
p_gb = pi_b * (1 - lam); q_G = 1 - p_gb; N = 1 / (1 - q_G**2)
f_GG_H = sp.simplify(2 * (1 - p_gb) / (2 - p_gb)); f_GG_BB = sp.simplify(p_gb / (2 - p_gb))

def return_step_distribution(x, entry):
    if entry == "H":
        a1 = pi_b + x * (1 - pi_b); a2 = pi_b * (1 - x)
    else:
        a1 = a2 = pi_b + x * (1 - pi_b)
    P_GG = (1 - a1) * (1 - a2); P_H = a1*(1-a2)+(1-a1)*a2; P_BB = a1*a2
    return sp.simplify(P_GG), sp.simplify(P_H), sp.simplify(P_BB)

def cycle_tau_R_P(n_entry, x_entry, entry):
    P_GG, P_H, P_BB = return_step_distribution(x_entry, entry)
    return_cost = sp.simplify(1 - P_GG)
    tau = sp.simplify((n_entry - 1) + 1 + P_GG * N)
    R = sp.simplify((n_entry - 1) * cost_a + return_cost + P_GG * 1 + 2 * c_switch_cold)
    P_to_H = sp.simplify(P_H + P_GG * f_GG_H); P_to_BB = sp.simplify(P_BB + P_GG * f_GG_BB)
    return tau, R, P_to_H, P_to_BB

tau_H, R_H, PHH, PHBB = cycle_tau_R_P(n_H, x_H, "H")
tau_BB, R_BB, PBBH, PBBBB = cycle_tau_R_P(n_BB, x_BB, "BB")
Pmat = sp.Matrix([[PHH, PHBB], [PBBH, PBBBB]])
nu_H_sym, nu_BB_sym = sp.symbols("nu_H nu_BB", positive=True)
nu_vec = sp.Matrix([[nu_H_sym, nu_BB_sym]])
eqs = list((nu_vec * Pmat - nu_vec)) + [nu_H_sym + nu_BB_sym - 1]
sol = sp.solve(eqs[:2] + [eqs[-1]], [nu_H_sym, nu_BB_sym], dict=True)
nu_H_val, nu_BB_val = sol[0][nu_H_sym], sol[0][nu_BB_sym]
g_cold_expr = sp.simplify((nu_H_val * R_H + nu_BB_val * R_BB) / (nu_H_val * tau_H + nu_BB_val * tau_BB))
g_cold_full = g_cold_expr.subs({x_H: lam**n_H, x_BB: lam**n_BB})
g_fn = sp.lambdify((pi_b, lam, cost_a, c_switch_cold, n_H, n_BB), g_cold_full, "numpy")

PI_B, LAM, C_SWITCH_COLD = 0.8, 0.85, 0.05
nH_vals = np.arange(2, 61)
nBB_vals = np.unique(np.concatenate([np.arange(2, 500), np.geomspace(500, 5_000_000, 800).astype(int)]))
NH, NBB = np.meshgrid(nH_vals, nBB_vals, indexing="ij")

for cost_a_val in [0.8991,0.8993,0.8995,0.8997,0.8999,0.8999+1e-4]:
    G = g_fn(PI_B, LAM, cost_a_val, C_SWITCH_COLD, NH, NBB)
    idx = np.unravel_index(np.argmin(G), G.shape)
    nh_star, nbb_star = nH_vals[idx[0]], nBB_vals[idx[1]]
    g_min = G[idx]
    beats = g_min < cost_a_val
    print(f"cost_a={cost_a_val:.5f}: argmin=({int(nh_star)},{int(nbb_star)}), g_min={g_min:.9f}, beats={beats}")
