"""Builds the full "where does warm dominate" phase diagram for the primary (pi_b<=1/2) active
cell under the pure-Gilbert idealization, per user request (2026-07-19): map the meta-parameter
combinations (pi_b, lambda, c_warm, c_switch_warm, c_switch_cold, cost_a) where warm-fixed beats
cold-fixed, not just report a single calibrated data point.

For each (pi_b, lambda), computes the FULL warm-win window in cost_a:
  - LOWER edge: exact closed form (already derived, warm_cold_phi_zero_closed_form_derivation_demo.py)
      cost_a_lo = c_warm/(1-pi_b)^2 + (1-q_G^2)*(1+2*c_switch_warm)
  - UPPER edge: found numerically (no closed form derived yet) -- the cost_a above which Phi
      returns to +c_warm (both regimes saturate to always-route-B).
  - Window depth: min(Phi) inside the window.

This is all EXACT (pure-Gilbert reduces to finite MDP + semi-Markov, no belief-grid resolution
error at all), unlike the general partial-observation case.

Run with: uv run --with sympy --with numpy python warm_win_phase_diagram_pure_gilbert_demo.py
"""

from __future__ import annotations

import numpy as np
import sympy as sp

from dmr import channels

ACTION_A, ACTION_B = 0, 1


# ---------- exact warm-side (8-state finite MDP, from pure_gilbert_finite_mdp_demo.py) ----------

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


# ---------- exact cold-side (symbolic g_cold_expr, lambdified, joint argmin) ----------

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


print("Building g_cold_expr symbolically (one-time)...")
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

_N_GRID = np.unique(np.concatenate([np.arange(2, 300), np.geomspace(300, 30_000, 150).astype(int)]))
_NH, _NBB = np.meshgrid(_N_GRID, _N_GRID, indexing="ij")


def solve_cold(pi_b_val, lam_val, cost_a_val, c_switch_cold_val):
    """BUG FIX (2026-07-19, caught via cross-check against the general belief-grid solver): the
    finite-park semi-Markov reduction only models cycles that eventually return to A, so its own
    joint-argmin ALWAYS eventually pays cost_a somewhere -- it cannot represent the OTHER
    degenerate policy, "always route B forever" (never return to A at all), which has value
    exactly the stationary path_b_loss = pi_b*(2-pi_b) for a symmetric pure-Gilbert pair. This
    third candidate becomes optimal whenever cost_a exceeds that stationary loss (routing A is
    then simply a bad deal on average, regardless of channel state) -- omitting it caused g_cold
    to be badly OVERESTIMATED (hence Phi underestimated / a fake wide-and-deep "window" reported)
    for every cost_a beyond the stationary loss. Must always take the min of THREE candidates:
    the finite-park optimum, the always-A-cold plateau (cost_a), and the always-B plateau
    (stationary path_b_loss)."""
    G = _g_cold_fn(pi_b_val, lam_val, cost_a_val, c_switch_cold_val, _NH, _NBB)
    g_min = float(np.min(G))
    stationary_path_b_loss = pi_b_val * (2 - pi_b_val)
    return min(g_min, cost_a_val, stationary_path_b_loss)


def cost_a_lo_closed_form(pi_b, lam, c_warm, c_switch_warm):
    p_gb = pi_b * (1 - lam)
    q_g = 1 - p_gb
    return c_warm / (1 - pi_b) ** 2 + (1 - q_g ** 2) * (1 + 2 * c_switch_warm)


def find_window(pi_b, lam, c_warm, c_switch_warm, c_switch_cold):
    """Returns (cost_a_lo, cost_a_hi, min_phi) or None if no window exists."""
    cost_a_lo_approx = cost_a_lo_closed_form(pi_b, lam, c_warm, c_switch_warm)
    p_gb = pi_b * (1 - lam)
    hop = channels.HopParams(p_gb=p_gb, p_bg=(1 - pi_b) * (1 - lam), eps_good=0.0, eps_bad=1.0)

    # sweep cost_a broadly around and beyond the closed-form lower edge to find both edges
    grid = np.unique(np.concatenate([
        np.linspace(0.3 * cost_a_lo_approx, cost_a_lo_approx, 6),
        np.linspace(cost_a_lo_approx, 3.0 * cost_a_lo_approx, 20),
        np.linspace(3.0 * cost_a_lo_approx, 6.0 * cost_a_lo_approx, 6),
    ]))
    phis = []
    for ca in grid:
        gw = solve_warm(hop, hop, float(ca), c_warm, c_switch_warm)
        gc = solve_cold(pi_b, lam, float(ca), c_switch_cold)
        phis.append(gw - gc)
    phis = np.array(phis)

    below = phis < -1e-9
    if not below.any():
        return None
    idx_below = np.where(below)[0]
    cost_a_lo = grid[idx_below[0]]
    cost_a_hi = grid[idx_below[-1]]
    return float(cost_a_lo), float(cost_a_hi), float(phis.min())


def main() -> None:
    C_WARM = 0.02
    C_SWITCH_WARM = 0.01
    C_SWITCH_COLD = 0.02

    print(f"=== Pure-Gilbert warm-win window map (c_warm={C_WARM}, "
          f"c_switch_warm={C_SWITCH_WARM}, c_switch_cold={C_SWITCH_COLD}) ===\n")
    print(f"{'pi_b':>6} {'lambda':>8} {'cost_a_lo':>11} {'cost_a_hi':>11} {'width':>9} "
          f"{'min_Phi':>10} {'depth/c_warm':>13}")

    for pi_b in [0.1, 0.2, 0.3, 0.4, 0.5]:
        for lam in [0.2, 0.4, 0.6, 0.8]:
            result = find_window(pi_b, lam, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)
            if result is None:
                print(f"{pi_b:>6.2f} {lam:>8.2f}  {'no window found':>34}")
            else:
                lo, hi, min_phi = result
                print(f"{pi_b:>6.2f} {lam:>8.2f} {lo:>11.5f} {hi:>11.5f} {hi-lo:>9.5f} "
                      f"{min_phi:>10.6f} {-min_phi/C_WARM*100:>11.1f}%")


if __name__ == "__main__":
    main()
