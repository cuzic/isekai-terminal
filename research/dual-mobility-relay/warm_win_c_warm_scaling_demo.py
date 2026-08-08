"""Checks how the warm-win window [cost_a_lo, cost_a_hi] and its depth scale with c_warm itself
(the other key meta-parameter, alongside pi_b/lambda already mapped in
warm_win_phase_diagram_pure_gilbert_demo.py), at a fixed representative (pi_b, lambda).

cost_a_lo is already known EXACTLY to scale linearly in c_warm (closed form,
warm_cold_phi_zero_closed_form_derivation_demo.py). This checks whether cost_a_hi and the depth
also scale simply (e.g. linearly) with c_warm, or whether the window's RELATIVE shape
(width/cost_a_lo, depth/c_warm) is roughly invariant to c_warm -- i.e. whether c_warm mostly just
sets the overall SCALE of the window without changing its qualitative shape.

Run with: uv run --with sympy python warm_win_c_warm_scaling_demo.py
"""

from __future__ import annotations

import numpy as np
import sympy as sp

from dmr import channels

ACTION_A, ACTION_B = 0, 1


def build_finite_mdp(hop1, hop2, cost_a, c_warm, c_switch_warm):
    T_channel = channels.joint_transition_matrix(hop1, hop2, rho=0.0)
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    predictive_path_b_loss = T_channel @ path_b_loss
    n = 8
    T = {ACTION_A: np.zeros((n, n)), ACTION_B: np.zeros((n, n))}
    cost = {ACTION_A: np.zeros(n), ACTION_B: np.zeros(n)}
    for c in range(4):
        for p in (0, 1):
            s = c * 2 + p
            for a in (ACTION_A, ACTION_B):
                route_loss = cost_a if a == ACTION_A else predictive_path_b_loss[c]
                switch = 0.0 if a == p else c_switch_warm
                cost[a][s] = route_loss + c_warm + switch
                for cp in range(4):
                    spp = cp * 2 + a
                    T[a][s, spp] = T_channel[c, cp]
    return T, cost


def solve_warm(hop1, hop2, cost_a, c_warm, c_switch_warm, ref_state=0, n_iters=50):
    T, cost = build_finite_mdp(hop1, hop2, cost_a, c_warm, c_switch_warm)
    n = 8
    policy = np.zeros(n, dtype=int)
    g = 0.0
    for it in range(n_iters):
        T_pi = np.array([T[policy[s]][s] for s in range(n)])
        c_pi = np.array([cost[policy[s]][s] for s in range(n)])
        A = np.zeros((n + 1, n + 1)); b = np.zeros(n + 1)
        A[:n, :n] = np.eye(n) - T_pi; A[:n, n] = 1.0; b[:n] = c_pi
        A[n, ref_state] = 1.0
        sol, *_ = np.linalg.lstsq(A, b, rcond=None)
        h, g = sol[:n], sol[n]
        q = np.stack([cost[a] + T[a] @ h for a in (ACTION_A, ACTION_B)], axis=1)
        new_policy = np.argmin(q, axis=1)
        if np.array_equal(new_policy, policy) and it > 0:
            break
        policy = new_policy
    return g


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
g_cold_fn = sp.lambdify((pi_b, lam, cost_a, c_switch_cold, n_H, n_BB), g_cold_full, "numpy")

N_GRID = np.unique(np.concatenate([np.arange(2, 300), np.geomspace(300, 30_000, 150).astype(int)]))
NH, NBB = np.meshgrid(N_GRID, N_GRID, indexing="ij")


def solve_cold(pi_b_val, lam_val, cost_a_val, c_switch_cold_val):
    """BUG FIX (2026-07-19, see warm_win_phase_diagram_pure_gilbert_demo.py's solve_cold for the
    full explanation): must also compare against the "always route B forever" plateau
    (stationary path_b_loss = pi_b*(2-pi_b)), not just the finite-park optimum and the
    "always-A-cold" plateau (cost_a) -- omitting it overestimates g_cold whenever cost_a exceeds
    the stationary loss."""
    G = g_cold_fn(pi_b_val, lam_val, cost_a_val, c_switch_cold_val, NH, NBB)
    stationary_path_b_loss = pi_b_val * (2 - pi_b_val)
    return min(float(np.min(G)), cost_a_val, stationary_path_b_loss)


def cost_a_lo_closed_form(pi_b_v, lam_v, c_warm_v, c_switch_warm_v):
    p_gb_v = pi_b_v * (1 - lam_v)
    q_g_v = 1 - p_gb_v
    return c_warm_v / (1 - pi_b_v) ** 2 + (1 - q_g_v ** 2) * (1 + 2 * c_switch_warm_v)


def find_window(pi_b_v, lam_v, c_warm_v, c_switch_warm_v, c_switch_cold_v):
    cost_a_lo_approx = cost_a_lo_closed_form(pi_b_v, lam_v, c_warm_v, c_switch_warm_v)
    hop = channels.HopParams(p_gb=pi_b_v * (1 - lam_v), p_bg=(1 - pi_b_v) * (1 - lam_v),
                              eps_good=0.0, eps_bad=1.0)
    grid = np.unique(np.concatenate([
        np.linspace(0.3 * cost_a_lo_approx, cost_a_lo_approx, 6),
        np.linspace(cost_a_lo_approx, 3.0 * cost_a_lo_approx, 20),
        np.linspace(3.0 * cost_a_lo_approx, 6.0 * cost_a_lo_approx, 6),
    ]))
    phis = []
    for ca in grid:
        gw = solve_warm(hop, hop, float(ca), c_warm_v, c_switch_warm_v)
        gc = solve_cold(pi_b_v, lam_v, float(ca), c_switch_cold_v)
        phis.append(gw - gc)
    phis = np.array(phis)
    below = phis < -1e-9
    if not below.any():
        return None
    idx = np.where(below)[0]
    return float(grid[idx[0]]), float(grid[idx[-1]]), float(phis.min())


def main() -> None:
    PI_B, LAM = 0.3, 0.4
    C_SWITCH_WARM = 0.01
    C_SWITCH_COLD = 0.02

    print(f"=== c_warm scaling at pi_b={PI_B}, lambda={LAM} ===\n")
    print(f"{'c_warm':>8} {'cost_a_lo':>11} {'cost_a_hi':>11} {'width':>9} {'min_Phi':>10} "
          f"{'depth/c_warm':>13} {'lo/c_warm':>10} {'hi/c_warm':>10}")
    for c_warm in [0.005, 0.01, 0.02, 0.04, 0.08]:
        result = find_window(PI_B, LAM, c_warm, C_SWITCH_WARM, C_SWITCH_COLD)
        if result is None:
            print(f"{c_warm:>8.3f}  no window found")
        else:
            lo, hi, min_phi = result
            print(f"{c_warm:>8.3f} {lo:>11.5f} {hi:>11.5f} {hi-lo:>9.5f} {min_phi:>10.6f} "
                  f"{-min_phi/c_warm*100:>12.1f}% {lo/c_warm:>10.3f} {hi/c_warm:>10.3f}")


if __name__ == "__main__":
    main()
