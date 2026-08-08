"""Follow-up to berlin_v2x_block_fit_demo.py: that script fixed c_warm=0.02 and only swept
c_switch_cold, finding gain EXACTLY 0.00% everywhere (g_adapt=g_cold at every point tried,
meaning warm standby itself was never worth it at c_warm=0.02 for this real hop pair, so the
decomposed/adaptive policy degenerates to the same value as simple cold switching). Before
concluding "no decomposition value for this real pair", do the full 2D (c_warm, c_switch_cold)
sweep this project's own established convention uses (real_channel_adaptivity_sweep_demo.py) --
c_warm=0.02 might simply be outside this pair's effective range, the way the paper's own
Table 4 shows only a narrow c_warm band gives non-negligible gain.

Run with: uv run python berlin_v2x_2d_sweep_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

HOP1 = channels.HopParams(p_gb=0.1909, p_bg=0.4553, eps_good=0.0320, eps_bad=0.3010)  # car4->car2, lambda=+0.354
HOP2 = channels.HopParams(p_gb=0.2764, p_bg=0.3933, eps_good=0.0695, eps_bad=0.4253)  # car3->car1, lambda=+0.330

COST_A = 0.30  # comparable to this pair's own stationary path-B loss (~0.304), see berlin_v2x_block_fit_demo.py
C_SWITCH_WARM = 0.01
RESOLUTION = 50
N_ITERS = 1500

C_WARM_VALUES = [0.002, 0.005, 0.010, 0.020, 0.040, 0.070, 0.100, 0.150]
C_SWITCH_COLD_VALUES = [0.02, 0.05, 0.10, 0.20, 0.35, 0.50, 0.70, 1.00]


def solve_point(c_warm: float, c_switch_cold: float) -> tuple[float, float, float]:
    path_b_loss = channels.path_b_loss_prob(HOP1, HOP2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, c_warm, C_SWITCH_WARM, c_switch_cold)
    sol_adapt = beliefgrid2d.belief_grid2d_value_iteration_warm(HOP1, HOP2, cost, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_warm = switching_curves.always_warm_value_iteration(HOP1, HOP2, COST_A, c_warm, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(HOP1, HOP2, COST_A, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_adapt.g, sol_warm.g, sol_cold.g


def main() -> None:
    print(f"=== Berlin V2X real pair: hop1 lambda={1-HOP1.p_gb-HOP1.p_bg:+.3f}, "
          f"hop2 lambda={1-HOP2.p_gb-HOP2.p_bg:+.3f}, cost_a={COST_A} ===\n")

    grid = np.zeros((len(C_WARM_VALUES), len(C_SWITCH_COLD_VALUES)))
    for i, c_warm in enumerate(C_WARM_VALUES):
        row = []
        for j, c_switch_cold in enumerate(C_SWITCH_COLD_VALUES):
            g_adapt, g_warm, g_cold = solve_point(c_warm, c_switch_cold)
            baseline = min(g_warm, g_cold)
            rel_value = (baseline - g_adapt) / baseline if baseline > 0 else 0.0
            grid[i, j] = rel_value
            row.append(f"{rel_value * 100:7.3f}")
        print(f"c_warm={c_warm:.3f}: " + " ".join(row))

    max_idx = np.unravel_index(np.argmax(grid), grid.shape)
    max_val = grid[max_idx]
    print(f"\nmax relative value: {max_val*100:.2f}% at c_warm={C_WARM_VALUES[max_idx[0]]}, "
          f"c_switch_cold={C_SWITCH_COLD_VALUES[max_idx[1]]}")
    above_5pct = [(C_WARM_VALUES[i], C_SWITCH_COLD_VALUES[j])
                  for i in range(len(C_WARM_VALUES)) for j in range(len(C_SWITCH_COLD_VALUES))
                  if grid[i, j] > 0.05]
    print(f"points clearing 5%: {above_5pct if above_5pct else '(none)'}")


if __name__ == "__main__":
    main()
