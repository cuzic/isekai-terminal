"""Quantifies the approximation error of the "distilled" leading-order closed form for the
c_warm-vanishing ceiling, across the FULL (pi_b, lambda) phase diagram -- not just the single
test point (pi_b=0.3, lambda=0.4) checked during its derivation, where it was found to
undershoot the true value by ~8%.

Leading-order closed form (see WARM_COLD_PURE_GILBERT_NOTES.md / THRESHOLD_PROOF.md sec 11.5):
  c_warm_vanish_leading = (1-pi_b)^2 * (cost_a_boundary - K)
  K = (1-q_G^2)*(1+2*c_switch_warm)
  cost_a_boundary = min(
      cold finite-park detachment: (1+2*c_switch_cold)/(1+N*(1-pi_b)^2),
      cold -> always-B: pi_b*(2-pi_b),
      warm route-B-iff-GG -> always-B: 1 - ((1-pi_b)^2/(pi_b*(2-pi_b)))*K
  )

True value obtained via the EXACT linear law (Phi(cost_a;c_warm)=c_warm+Psi(cost_a) is additive
in c_warm, bug-fixed cold solver, pure-Gilbert reduction -- no belief-grid resolution error at
all): probe at a small c_warm, find Psi's numeric minimum over a wide cost_a search, negate.

Run with: uv run --with sympy python c_warm_vanish_approximation_error_demo.py  (~10-15 min)
"""

from __future__ import annotations

import numpy as np
import sympy as sp

C_SWITCH_WARM = 0.01
C_SWITCH_COLD = 0.02
C_WARM_PROBE = 0.001  # small enough to be a clean probe of Psi, per the linear-law identity

pi_b, lam, cost_a, c_switch_cold = sp.symbols("pi_b lambda cost_a c_switch_cold", positive=True)
n_H, n_BB = sp.symbols("n_H n_BB", positive=True, integer=True)
x_H, x_BB = sp.symbols("x_H x_BB", positive=True)
p_gb = pi_b * (1 - lam)
q_G = 1 - p_gb
N = 1 / (1 - q_G**2)
f_GG_H = sp.simplify(2 * (1 - p_gb) / (2 - p_gb))
f_GG_BB = sp.simplify(p_gb / (2 - p_gb))


def return_step_distribution(x, entry):
    if entry == "H":
        a1 = pi_b + x * (1 - pi_b)
        a2 = pi_b * (1 - x)
    else:
        a1 = a2 = pi_b + x * (1 - pi_b)
    P_GG = (1 - a1) * (1 - a2)
    P_H = a1 * (1 - a2) + (1 - a1) * a2
    P_BB = a1 * a2
    return sp.simplify(P_GG), sp.simplify(P_H), sp.simplify(P_BB)


def cycle_tau_R_P(n_entry, x_entry, entry):
    P_GG, P_H, P_BB = return_step_distribution(x_entry, entry)
    return_cost = sp.simplify(1 - P_GG)
    tau = sp.simplify((n_entry - 1) + 1 + P_GG * N)
    R = sp.simplify((n_entry - 1) * cost_a + return_cost + P_GG * 1 + 2 * c_switch_cold)
    P_to_H = sp.simplify(P_H + P_GG * f_GG_H)
    P_to_BB = sp.simplify(P_BB + P_GG * f_GG_BB)
    return tau, R, P_to_H, P_to_BB


print("Building g_cold_expr symbolically (one-time)...")
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
g_cold_fn = sp.lambdify((pi_b, lam, cost_a, c_switch_cold, n_H, n_BB), g_cold_full, "numpy")
print("Done.\n")

N_GRID = np.unique(np.concatenate([np.arange(2, 300), np.geomspace(300, 30_000, 150).astype(int)]))
NH, NBB = np.meshgrid(N_GRID, N_GRID, indexing="ij")


def solve_cold(pi_b_val, lam_val, cost_a_val, c_switch_cold_val):
    G = g_cold_fn(pi_b_val, lam_val, cost_a_val, c_switch_cold_val, NH, NBB)
    g_min = float(np.min(G))
    stationary_path_b_loss = pi_b_val * (2 - pi_b_val)
    return min(g_min, cost_a_val, stationary_path_b_loss)


def g_warm_route_b_iff_gg(pi_b_val, lam_val, cost_a_val, c_warm_val, c_switch_warm_val):
    p_gb_val = pi_b_val * (1 - lam_val)
    q_g_val = 1 - p_gb_val
    a2 = (1 - pi_b_val) ** 2
    K = (1 - q_g_val ** 2) * (1 + 2 * c_switch_warm_val)
    return c_warm_val + a2 * K + (1 - a2) * cost_a_val


def leading_order_c_warm_vanish(pi_b_val, lam_val, c_switch_warm_val, c_switch_cold_val):
    p_gb_val = pi_b_val * (1 - lam_val)
    q_g_val = 1 - p_gb_val
    a2 = (1 - pi_b_val) ** 2
    b2 = pi_b_val * (2 - pi_b_val)
    N_val = 1 / (1 - q_g_val ** 2)
    K = (1 - q_g_val ** 2) * (1 + 2 * c_switch_warm_val)

    cand_detach = (1 + 2 * c_switch_cold_val) / (1 + N_val * a2)
    cand_cold_alwaysB = b2
    cand_warm_alwaysB = 1 - (a2 / b2) * K
    cost_a_boundary = min(cand_detach, cand_cold_alwaysB, cand_warm_alwaysB)
    return a2 * (cost_a_boundary - K), cost_a_boundary


def true_c_warm_vanish(pi_b_val, lam_val, c_switch_warm_val, c_switch_cold_val,
                        cost_a_boundary):
    """Find Psi's minimum by a fine cost_a search anchored near cost_a_boundary (the primary
    cell's own upper edge, in cost_a units -- NOT the c_warm_vanish estimate, a different unit
    entirely; conflating the two was a real bug in an earlier version of this script, caught by
    the implausible negative "true" values it produced). Search generously beyond the naive
    boundary too, since the true valley bottom is already known to sit meaningfully further out
    (~9% beyond the naive detachment point in the original single-point check)."""
    lo = 0.3 * cost_a_boundary
    hi = 2.5 * cost_a_boundary
    grid = np.linspace(max(lo, 0.005), hi, 80)
    best_psi = float("inf")
    for ca in grid:
        gw = g_warm_route_b_iff_gg(pi_b_val, lam_val, float(ca), C_WARM_PROBE, c_switch_warm_val)
        gc = solve_cold(pi_b_val, lam_val, float(ca), c_switch_cold_val)
        psi = (gw - C_WARM_PROBE) - gc
        best_psi = min(best_psi, psi)
    return -best_psi


def main() -> None:
    print(f"{'pi_b':>6} {'lambda':>8} {'leading_order':>14} {'true(numeric)':>14} "
          f"{'rel_err%':>9} {'boundary_used':>14}")
    for pi_b_val in [0.1, 0.2, 0.3, 0.4, 0.5]:
        for lam_val in [0.2, 0.4, 0.6, 0.8]:
            leading, boundary = leading_order_c_warm_vanish(
                pi_b_val, lam_val, C_SWITCH_WARM, C_SWITCH_COLD)
            if leading <= 0:
                print(f"{pi_b_val:>6.2f} {lam_val:>8.2f}  leading_order<=0, no window predicted")
                continue
            true_val = true_c_warm_vanish(pi_b_val, lam_val, C_SWITCH_WARM, C_SWITCH_COLD, boundary)
            rel_err = (true_val - leading) / true_val * 100 if true_val > 0 else float("nan")
            print(f"{pi_b_val:>6.2f} {lam_val:>8.2f} {leading:>14.6f} {true_val:>14.6f} "
                  f"{rel_err:>8.1f}% {boundary:>14.5f}")


if __name__ == "__main__":
    main()
