import numpy as np
from dmr import channels, switching_curves

PI_B, LAM = 0.1, 0.4
C_WARM, C_SWITCH_WARM, C_SWITCH_COLD = 0.02, 0.01, 0.02
P_GB, P_BG = PI_B*(1-LAM), (1-PI_B)*(1-LAM)
hop = channels.HopParams(p_gb=P_GB, p_bg=P_BG, eps_good=0.0, eps_bad=1.0)

print("stationary path_b_loss =", PI_B*(2-PI_B))
print()

# general (belief-grid) solver at various cost_a, high resolution
for ca in [0.60, 0.65, 0.688, 0.70, 0.75, 0.80, 0.85, 0.861, 0.90]:
    sol_warm = switching_curves.always_warm_value_iteration(hop, hop, ca, C_WARM, C_SWITCH_WARM, resolution=150, n_iters=3000)
    sol_cold = switching_curves.always_cold_value_iteration(hop, hop, ca, C_SWITCH_COLD, resolution=150, n_iters=3000)
    print(f"cost_a={ca:.3f}: g_warm={sol_warm.g:.6f}, g_cold={sol_cold.g:.6f}, Phi={sol_warm.g-sol_cold.g:+.6f}")
