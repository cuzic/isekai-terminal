"""Numerically locate the Phi=0 (g_warm*=g_cold*) boundary for the symmetric pure-Gilbert slice,
per opus-symbolic-advisor's recommended kickoff plan (2026-07-19): trace the boundary numerically
first (both sides computed via EXACT finite algorithms -- warm via 8-state policy iteration,
cold via joint (n_H,n_BB) argmin including the plateau candidate g=cost_a), identify which
warm-policy / cold-regime pair is ACTIVE at the boundary, THEN attempt a closed form only for
that active cell (not a monster covering all policy pairs).

Default case targeted: pi_b<=1/2 (matches real Berlin V2X calibration, both hops pi_b<1/2) --
per the advisor, this collapses both cold entries (H, BB) to the SAME monotone saturation
threshold cost_a* = (1+2*c_switch_cold)/(1+N*(1-pi_b)^2), no interior-optimum window, no
multimodality -- the simple default case this script targets.

Run with: uv run --with sympy --with numpy python warm_cold_phi_zero_active_cell_demo.py
"""

from __future__ import annotations

import numpy as np
import sympy as sp

from dmr import channels

ACTION_A, ACTION_B = 0, 1


# ---------- warm side: exact 8-state finite MDP (from pure_gilbert_finite_mdp_demo.py) ----------

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


def solve_average_cost_policy_iteration(T, cost, ref_state=0, n_iters=50):
    n = len(next(iter(cost.values())))
    policy = np.zeros(n, dtype=int)
    for it in range(n_iters):
        T_pi = np.array([T[policy[s]][s] for s in range(n)])
        c_pi = np.array([cost[policy[s]][s] for s in range(n)])
        A = np.zeros((n + 1, n + 1))
        b = np.zeros(n + 1)
        A[:n, :n] = np.eye(n) - T_pi
        A[:n, n] = 1.0
        b[:n] = c_pi
        A[n, ref_state] = 1.0
        sol, *_ = np.linalg.lstsq(A, b, rcond=None)
        h, g = sol[:n], sol[n]
        q = np.stack([cost[a] + T[a] @ h for a in (ACTION_A, ACTION_B)], axis=1)
        new_policy = np.argmin(q, axis=1)
        if np.array_equal(new_policy, policy) and it > 0:
            return g, policy
        policy = new_policy
    return g, policy


def classify_warm_policy(policy):
    """policy indexed by state=c*2+p, c in {GG,GB,BG,BB}, p in {A,B}. Returns a short label."""
    actions_by_c = [policy[c * 2 + 0] for c in range(4)]  # action when arriving from context A (any c)
    if all(a == ACTION_A for a in actions_by_c):
        return "always-A"
    if all(a == ACTION_B for a in actions_by_c):
        return "always-B"
    if actions_by_c == [ACTION_B, ACTION_A, ACTION_A, ACTION_A]:
        return "route-B-iff-GG"
    return f"other:{actions_by_c}"


# ---------- cold side: exact joint (n_H, n_BB) argmin incl. plateau (validated g_cold_expr) ----------

_pi_b, _lam, _cost_a, _c_switch_cold = sp.symbols("pi_b lambda cost_a c_switch_cold", positive=True)
_n_H, _n_BB = sp.symbols("n_H n_BB", positive=True, integer=True)
_x_H, _x_BB = sp.symbols("x_H x_BB", positive=True)

_p_gb = _pi_b * (1 - _lam)
_q_G = 1 - _p_gb
_N = 1 / (1 - _q_G**2)
_f_GG_H = sp.simplify(2 * (1 - _p_gb) / (2 - _p_gb))
_f_GG_BB = sp.simplify(_p_gb / (2 - _p_gb))


def _return_step_distribution(x, entry):
    if entry == "H":
        a1 = _pi_b + x * (1 - _pi_b)
        a2 = _pi_b * (1 - x)
    else:
        a1 = a2 = _pi_b + x * (1 - _pi_b)
    P_GG = (1 - a1) * (1 - a2)
    P_H = a1 * (1 - a2) + (1 - a1) * a2
    P_BB = a1 * a2
    return sp.simplify(P_GG), sp.simplify(P_H), sp.simplify(P_BB)


def _cycle_tau_R_P(n_entry, x_entry, entry):
    P_GG, P_H, P_BB = _return_step_distribution(x_entry, entry)
    return_cost = sp.simplify(1 - P_GG)
    tau = sp.simplify((n_entry - 1) + 1 + P_GG * _N)
    R = sp.simplify((n_entry - 1) * _cost_a + return_cost + P_GG * 1 + 2 * _c_switch_cold)
    P_to_H = sp.simplify(P_H + P_GG * _f_GG_H)
    P_to_BB = sp.simplify(P_BB + P_GG * _f_GG_BB)
    return tau, R, P_to_H, P_to_BB


print("Building g_cold_expr symbolically (one-time, shared across the whole sweep)...")
_tau_H, _R_H, _PHH, _PHBB = _cycle_tau_R_P(_n_H, _x_H, "H")
_tau_BB, _R_BB, _PBBH, _PBBBB = _cycle_tau_R_P(_n_BB, _x_BB, "BB")
_Pmat = sp.Matrix([[_PHH, _PHBB], [_PBBH, _PBBBB]])
_nu_H_sym, _nu_BB_sym = sp.symbols("nu_H nu_BB", positive=True)
_nu_vec = sp.Matrix([[_nu_H_sym, _nu_BB_sym]])
_eqs = list((_nu_vec * _Pmat - _nu_vec)) + [_nu_H_sym + _nu_BB_sym - 1]
_sol = sp.solve(_eqs[:2] + [_eqs[-1]], [_nu_H_sym, _nu_BB_sym], dict=True)
_nu_H_val, _nu_BB_val = _sol[0][_nu_H_sym], _sol[0][_nu_BB_sym]
_g_cold_expr = sp.simplify((_nu_H_val * _R_H + _nu_BB_val * _R_BB) / (_nu_H_val * _tau_H + _nu_BB_val * _tau_BB))
_g_cold_full = _g_cold_expr.subs({_x_H: _lam**_n_H, _x_BB: _lam**_n_BB})
_g_cold_fn = sp.lambdify((_pi_b, _lam, _cost_a, _c_switch_cold, _n_H, _n_BB), _g_cold_full, "numpy")
print("Done.\n")

_N_GRID = np.unique(np.concatenate([np.arange(2, 500), np.geomspace(500, 500_000, 300).astype(int)]))
_NH, _NBB = np.meshgrid(_N_GRID, _N_GRID, indexing="ij")


def solve_cold_joint(pi_b_val, lam_val, cost_a_val, c_switch_cold_val):
    """Returns (g_cold_star, regime_label, (n_H*, n_BB*) or None if plateau)."""
    G = _g_cold_fn(pi_b_val, lam_val, cost_a_val, c_switch_cold_val, _NH, _NBB)
    idx = np.unravel_index(np.argmin(G), G.shape)
    nh_star, nbb_star = _N_GRID[idx[0]], _N_GRID[idx[1]]
    g_min = G[idx]
    if g_min < cost_a_val - 1e-9:
        return g_min, "finite-park", (int(nh_star), int(nbb_star))
    return cost_a_val, "plateau(always-A-cold)", None


def main():
    PI_B = 0.3  # real-data-like default: pi_b<=1/2
    LAM = 0.4
    C_WARM = 0.02
    C_SWITCH_WARM = 0.01
    C_SWITCH_COLD = 0.05

    p_gb = PI_B * (1 - LAM)
    p_bg = (1 - PI_B) * (1 - LAM)
    hop = channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=0.0, eps_bad=1.0)
    stationary_loss = PI_B * (2 - PI_B)

    print(f"=== Phi=0 boundary sweep: pi_b={PI_B}, lambda={LAM}, c_warm={C_WARM}, "
          f"c_switch_warm={C_SWITCH_WARM}, c_switch_cold={C_SWITCH_COLD} ===")
    print(f"stationary path-B loss = {stationary_loss:.4f}\n")

    print(f"{'cost_a':>8}  {'g_warm*':>10}  {'warm policy':>16}  {'g_cold*':>10}  {'cold regime':>22}  {'Phi=warm-cold':>14}")
    for cost_a_val in np.linspace(0.05, stationary_loss * 0.98, 25):
        T, cost = build_finite_mdp(hop, hop, cost_a_val, C_WARM, C_SWITCH_WARM)
        g_warm, policy = solve_average_cost_policy_iteration(T, cost)
        warm_label = classify_warm_policy(policy)

        g_cold, cold_label, ncold = solve_cold_joint(PI_B, LAM, cost_a_val, C_SWITCH_COLD)

        phi = g_warm - g_cold
        print(f"{cost_a_val:>8.4f}  {g_warm:>10.6f}  {warm_label:>16}  {g_cold:>10.6f}  "
              f"{cold_label:>22}  {phi:>+14.6f}")


if __name__ == "__main__":
    main()
