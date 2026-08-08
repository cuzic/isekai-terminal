"""Check whether the pi_b>1/2 interior-GLOBAL-optimum window (confirmed in the single-entry toy
model, see WARM_COLD_PURE_GILBERT_NOTES.md) also appears in the FULL 2-state embedded chain
g_cold(n_H, n_BB) -- not just the single-entry simplification. Self-directed follow-up to the
question sent to opus-symbolic-advisor (2026-07-19): "does the same window survive in the full
system, or does the toy model's finding not transfer because n_H and n_BB interact through the
shared embedded-chain stationary weights nu?"

Rebuilds g_cold_expr exactly as in pure_gilbert_symbolic_cold_demo.py (already validated there to
diff<1e-9 against the continuous solver at 2 points), then lambdifies it and does a full 2D grid
search over (n_H, n_BB) for each cost_a in a sweep, tracking where the joint global argmin leaves
the "both huge" (plateau/park-forever) corner for a genuine finite interior point.

Run with: uv run --with sympy --with numpy python pure_gilbert_cold_full_embedded_sweep_demo.py
"""

from __future__ import annotations

import numpy as np
import sympy as sp

pi_b, lam, cost_a, c_switch_cold = sp.symbols("pi_b lambda cost_a c_switch_cold", positive=True)
n_H, n_BB = sp.symbols("n_H n_BB", positive=True, integer=True)
x_H, x_BB = sp.symbols("x_H x_BB", positive=True)

p_gb = pi_b * (1 - lam)
q_G = 1 - p_gb
N = 1 / (1 - q_G**2)
f_GG_H = sp.simplify(2 * (1 - p_gb) / (2 - p_gb))
f_GG_BB = sp.simplify(p_gb / (2 - p_gb))


def return_step_distribution(x, entry: str):
    if entry == "H":
        a1 = pi_b + x * (1 - pi_b)
        a2 = pi_b * (1 - x)
    else:
        a1 = a2 = pi_b + x * (1 - pi_b)
    P_GG = (1 - a1) * (1 - a2)
    P_H = a1 * (1 - a2) + (1 - a1) * a2
    P_BB = a1 * a2
    return sp.simplify(P_GG), sp.simplify(P_H), sp.simplify(P_BB)


def cycle_tau_R_P(n_entry, x_entry, entry: str):
    P_GG, P_H, P_BB = return_step_distribution(x_entry, entry)
    return_cost = sp.simplify(1 - P_GG)
    tau = sp.simplify((n_entry - 1) + 1 + P_GG * N)
    R = sp.simplify((n_entry - 1) * cost_a + return_cost + P_GG * 1 + 2 * c_switch_cold)
    P_to_H = sp.simplify(P_H + P_GG * f_GG_H)
    P_to_BB = sp.simplify(P_BB + P_GG * f_GG_BB)
    return tau, R, P_to_H, P_to_BB


print("Rebuilding g_cold_expr symbolically (matches pure_gilbert_symbolic_cold_demo.py)...")
tau_H, R_H, PHH, PHBB = cycle_tau_R_P(n_H, x_H, "H")
tau_BB, R_BB, PBBH, PBBBB = cycle_tau_R_P(n_BB, x_BB, "BB")

Pmat = sp.Matrix([[PHH, PHBB], [PBBH, PBBBB]])
nu_H_sym, nu_BB_sym = sp.symbols("nu_H nu_BB", positive=True)
nu_vec = sp.Matrix([[nu_H_sym, nu_BB_sym]])
eqs = list((nu_vec * Pmat - nu_vec)) + [nu_H_sym + nu_BB_sym - 1]
sol = sp.solve(eqs[:2] + [eqs[-1]], [nu_H_sym, nu_BB_sym], dict=True)
nu_H_val, nu_BB_val = sol[0][nu_H_sym], sol[0][nu_BB_sym]
g_cold_expr = sp.simplify((nu_H_val * R_H + nu_BB_val * R_BB) / (nu_H_val * tau_H + nu_BB_val * tau_BB))

# Substitute x_H = lam**n_H, x_BB = lam**n_BB symbolically before lambdify, so the numeric
# function takes (pi_b, lam, cost_a, c_switch_cold, n_H, n_BB) directly.
g_cold_full = g_cold_expr.subs({x_H: lam**n_H, x_BB: lam**n_BB})
g_fn = sp.lambdify((pi_b, lam, cost_a, c_switch_cold, n_H, n_BB), g_cold_full, "numpy")

print("Lambdified. Running 2D grid sweep...\n")

PI_B = 0.8
LAM = 0.85
C_SWITCH_COLD = 0.05
N_MAX = 200  # search n_H, n_BB in [2, N_MAX]

n_vals = np.arange(2, N_MAX + 1)
NH, NBB = np.meshgrid(n_vals, n_vals, indexing="ij")

print(f"pi_b={PI_B}, lambda={LAM}, c_switch_cold={C_SWITCH_COLD}, "
      f"stationary path-B loss={PI_B*(2-PI_B):.4f}\n")
print(f"{'cost_a':>8}  {'argmin(n_H,n_BB)':>18}  {'g_min':>12}  {'at boundary?':>14}")

for cost_a_val in [0.50, 0.70, 0.80, 0.84, 0.86, 0.88, 0.90, 0.92, 0.94, 0.96]:
    G = g_fn(PI_B, LAM, cost_a_val, C_SWITCH_COLD, NH, NBB)
    idx = np.unravel_index(np.argmin(G), G.shape)
    nh_star, nbb_star = n_vals[idx[0]], n_vals[idx[1]]
    g_min = G[idx]
    at_boundary = (nh_star == N_MAX or nbb_star == N_MAX)
    print(f"{cost_a_val:>8.2f}  {(int(nh_star), int(nbb_star))!s:>18}  {g_min:>12.6f}  {str(at_boundary):>14}")

print("\nIf argmin relocates from the N_MAX boundary to a genuine interior (n_H,n_BB) pair as")
print("cost_a crosses some threshold (mirroring the single-entry toy's 0.84->0.86 transition),")
print("the window is CONFIRMED in the full embedded system, not just the toy simplification.")
