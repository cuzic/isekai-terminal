import numpy as np
import sympy as sp
from dmr import channels

ACTION_A, ACTION_B = 0, 1
PI_B, LAM = 0.3, 0.4
C_SWITCH_WARM, C_SWITCH_COLD = 0.01, 0.02

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
            return g, policy
        policy = new_policy
    return g, policy

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
g_cold_fn = sp.lambdify((pi_b, lam, cost_a, c_switch_cold, n_H, n_BB), g_cold_full, "numpy")

N_GRID = np.unique(np.concatenate([np.arange(2, 300), np.geomspace(300, 30_000, 150).astype(int)]))
NH, NBB = np.meshgrid(N_GRID, N_GRID, indexing="ij")

def solve_cold(pi_b_val, lam_val, cost_a_val, c_switch_cold_val):
    G = g_cold_fn(pi_b_val, lam_val, cost_a_val, c_switch_cold_val, NH, NBB)
    g_min = float(np.min(G))
    stationary_path_b_loss = pi_b_val * (2 - pi_b_val)
    return min(g_min, cost_a_val, stationary_path_b_loss)

hop = channels.HopParams(p_gb=PI_B*(1-LAM), p_bg=(1-PI_B)*(1-LAM), eps_good=0.0, eps_bad=1.0)

# Psi(cost_a) = Phi(cost_a; c_warm) - c_warm, should be c_warm-independent. Use small c_warm=0.001
# to minimize its own numerical footprint, then just report g_warm - c_warm - g_cold as Psi.
C_WARM_PROBE = 0.001
print(f"{'cost_a':>8} {'g_warm':>10} {'g_cold':>10} {'Psi=Phi-c_warm':>15}")
best_ca, best_psi = None, float('inf')
for ca in np.linspace(0.40, 0.46, 31):
    gw, policy = solve_warm(hop, hop, ca, C_WARM_PROBE, C_SWITCH_WARM)
    gc = solve_cold(PI_B, LAM, ca, C_SWITCH_COLD)
    psi = (gw - C_WARM_PROBE) - gc
    if psi < best_psi:
        best_psi, best_ca = psi, ca
    print(f"{ca:>8.4f} {gw:>10.6f} {gc:>10.6f} {psi:>15.6f}")

print(f"\nvalley bottom: cost_a={best_ca:.4f}, Psi_min={best_psi:.6f}, "
      f"=> c_warm_vanish = -Psi_min = {-best_psi:.6f}")
