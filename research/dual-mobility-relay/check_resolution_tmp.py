from dmr import channels, switching_curves

PI_B, LAM = 0.1, 0.4
C_WARM, C_SWITCH_WARM, C_SWITCH_COLD = 0.02, 0.01, 0.02
P_GB, P_BG = PI_B*(1-LAM), (1-PI_B)*(1-LAM)

def phi(cost_a, resolution, n_iters):
    hop = channels.HopParams(p_gb=P_GB, p_bg=P_BG, eps_good=0.0, eps_bad=1.0)
    sol_warm = switching_curves.always_warm_value_iteration(hop, hop, cost_a, C_WARM, C_SWITCH_WARM, resolution=resolution, n_iters=n_iters)
    sol_cold = switching_curves.always_cold_value_iteration(hop, hop, cost_a, C_SWITCH_COLD, resolution=resolution, n_iters=n_iters)
    return sol_warm.g, sol_cold.g, sol_warm.g - sol_cold.g

for res in [60, 100, 150, 250]:
    for ca in [0.70, 0.75, 0.80]:
        gw, gc, ph = phi(ca, res, 2000)
        print(f"resolution={res}, cost_a={ca}: g_warm={gw:.6f}, g_cold={gc:.6f}, Phi={ph:+.6f}")
    print()
