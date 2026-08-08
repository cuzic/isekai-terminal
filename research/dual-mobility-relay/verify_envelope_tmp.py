import numpy as np
import sympy as sp

PI_B, LAM, C_SWITCH_COLD = 0.3, 0.4, 0.02
COST_A_VALLEY = 0.456  # approx valley bottom found numerically

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

# Also need nu_H, nu_BB, tau_H, tau_BB as numeric functions to compute A-fraction
nu_H_full = nu_H_val.subs({x_H: lam**n_H, x_BB: lam**n_BB})
nu_BB_full = nu_BB_val.subs({x_H: lam**n_H, x_BB: lam**n_BB})
tau_H_full = tau_H.subs({x_H: lam**n_H})
tau_BB_full = tau_BB.subs({x_BB: lam**n_BB})
nu_H_fn = sp.lambdify((pi_b, lam, n_H, n_BB), nu_H_full, "numpy")
nu_BB_fn = sp.lambdify((pi_b, lam, n_H, n_BB), nu_BB_full, "numpy")
tau_H_fn = sp.lambdify((pi_b, lam, n_H), tau_H_full, "numpy")
tau_BB_fn = sp.lambdify((pi_b, lam, n_BB), tau_BB_full, "numpy")

N_GRID = np.unique(np.concatenate([np.arange(2, 300), np.geomspace(300, 30_000, 150).astype(int)]))
NH, NBB = np.meshgrid(N_GRID, N_GRID, indexing="ij")

G = g_cold_fn(PI_B, LAM, COST_A_VALLEY, C_SWITCH_COLD, NH, NBB)
idx = np.unravel_index(np.argmin(G), G.shape)
n_h_star, n_bb_star = N_GRID[idx[0]], N_GRID[idx[1]]
g_min = G[idx]
stationary_loss = PI_B * (2 - PI_B)
g_cold_true = min(g_min, COST_A_VALLEY, stationary_loss)

print(f"At cost_a={COST_A_VALLEY}: optimal (n_H*,n_BB*)=({n_h_star},{n_bb_star}), g_min={g_min:.6f}, "
      f"g_cold(with plateau candidates)={g_cold_true:.6f}")

nu_h = nu_H_fn(PI_B, LAM, n_h_star, n_bb_star)
nu_bb = nu_BB_fn(PI_B, LAM, n_h_star, n_bb_star)
tau_h = tau_H_fn(PI_B, LAM, n_h_star)
tau_bb = tau_BB_fn(PI_B, LAM, n_bb_star)

A_time = nu_h * (n_h_star - 1) + nu_bb * (n_bb_star - 1)
total_time = nu_h * tau_h + nu_bb * tau_bb
A_fraction_cold = A_time / total_time

b_squared = PI_B * (2 - PI_B)
print(f"cold's A-dwelling fraction at optimum = {A_fraction_cold:.6f}")
print(f"b^2 = pi_b*(2-pi_b) = {b_squared:.6f}")
print(f"match? diff = {A_fraction_cold - b_squared:.6f}")
