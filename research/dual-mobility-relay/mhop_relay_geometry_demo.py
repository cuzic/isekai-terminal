"""Task #66 (C0): geometric re-analysis of the M=2-relay "flicker" -- where
does genuine stay-region disconnection (task #68's strict connected-
components check, not just the transition-count flicker check) actually
occur? Per Fable's 2026-07-18 suggestion: is it confined near triple points
where all 3 routes (A, R1, R2) are near-equally good, or is it more diffuse?

Uses the corrected worst-case witness from `mhop_relay_search_demo.py`'s
seed=2718 search (genuine 3+-transition flicker, not the earlier miscounted
80/150 witness -- see MHOP_RELAY_NOTES.md's correction).

Run with: uv run python mhop_relay_geometry_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, mhop_relay

# Corrected worst-case witness (genuine flicker, from the seed=2718 search with the
# off-by-one fix applied -- see MHOP_RELAY_NOTES.md).
RELAY1 = channels.HopParams(p_gb=0.018846684946415816, p_bg=0.1107606703446672,
                             eps_good=0.013130216214149122, eps_bad=0.44148946798907146)
RELAY2 = channels.HopParams(p_gb=0.0366049350393019, p_bg=0.03589818673588046,
                             eps_good=0.034764642885318026, eps_bad=0.5317276749980895)
COST_A, C_SWITCH = 0.2899, 0.2367


def main() -> None:
    resolution = 100
    sol = mhop_relay.mhop_relay_value_iteration(RELAY1, RELAY2, COST_A, C_SWITCH,
                                                 resolution=resolution, n_iters=2500)
    grid = sol.grid
    axis = grid.axis

    print("=== Locating genuine disconnections (task #68's strict check) per context ===")
    disconnection_locations = []  # (context, axis_name, fixed_coord, list of beta1/beta2 at run boundaries)
    for ctx in range(3):
        cc = mhop_relay.stay_region_connected_components_check(sol, ctx)
        print(f"context={['A', 'R1', 'R2'][ctx]}: {cc}")

        stay = (sol.policy[:, ctx] == ctx).astype(int).reshape(grid.shape)
        for j in range(stay.shape[1]):  # beta1-direction columns, fixed beta2
            if mhop_relay._count_true_runs(stay[:, j]) > 1:
                disconnection_locations.append((ctx, "beta1@fixed_beta2", axis[j]))
        for i in range(stay.shape[0]):  # beta2-direction rows, fixed beta1
            if mhop_relay._count_true_runs(stay[i, :]) > 1:
                disconnection_locations.append((ctx, "beta2@fixed_beta1", axis[i]))

    print(f"\n{len(disconnection_locations)} disconnected slices found total")

    print("\n=== Distance from each disconnected slice's fixed coordinate to the triple point's ===")
    print("=== matching coordinate (NOT the disconnected component's own location/midpoint) ===")
    print("(a 'triple point' is where all 3 routes' Q-values are near-equal -- q_A~q_R1~q_R2)")
    print("(this uses a switch-cost-FREE proxy for competitiveness, not the exact context-specific")
    print(" policy-switching boundary, which would need +c_switch on the off-diagonal terms --")
    print(" confirmed adequate as an exploratory proxy by Codex review, 2026-07-18)")
    q = sol.q  # (n_points, 3 contexts, 3 next-routes)
    # Triple-point proxy: at each grid point, the spread across the 3 routes' own "stay" cost
    # q[:, ctx, ctx] (i.e. base[route] with NO switch cost, since staying never pays one) -- their
    # spread measures how close the 3 routes are to equally good, ignoring switching costs.
    route_costs = np.stack([q[:, ctx, ctx] for ctx in range(3)], axis=1)  # (n_points, 3)
    spread = route_costs.max(axis=1) - route_costs.min(axis=1)
    spread_grid = spread.reshape(grid.shape)
    triple_point_idx = np.unravel_index(np.argmin(spread_grid), spread_grid.shape)
    triple_point = (float(axis[triple_point_idx[0]]), float(axis[triple_point_idx[1]]))
    print(f"closest approach to a triple point (min 3-way spread): beta=({triple_point[0]:.3f}, "
          f"{triple_point[1]:.3f}), spread={spread_grid[triple_point_idx]:.4e}")

    for ctx, axis_name, fixed_coord in disconnection_locations[:15]:
        if axis_name == "beta1@fixed_beta2":
            dist = abs(fixed_coord - triple_point[1])
        else:
            dist = abs(fixed_coord - triple_point[0])
        print(f"  context={['A', 'R1', 'R2'][ctx]}, {axis_name}={fixed_coord:.4f}: "
              f"distance to triple point's matching coordinate = {dist:.4f}")

    print("\n=== Verdict ===")
    if disconnection_locations:
        distances = []
        for ctx, axis_name, fixed_coord in disconnection_locations:
            ref = triple_point[1] if axis_name == "beta1@fixed_beta2" else triple_point[0]
            distances.append(abs(fixed_coord - ref))
        print(f"mean distance from disconnected slices to the triple-point coordinate: "
              f"{np.mean(distances):.4f} (grid spans [0,1], so <0.15 would suggest localization)")
        if np.mean(distances) < 0.15:
            print("Disconnections are concentrated NEAR the triple point -- consistent with Fable's")
            print("hypothesis that flicker occurs where all 3 routes are near-equally competitive.")
        else:
            print("Disconnections are NOT particularly close to the triple point -- the flicker")
            print("geometry is more diffuse than a simple 'near triple points only' story.")
    else:
        print("No genuine disconnections found in this witness at this resolution (unexpected --")
        print("mhop_relay_search_demo.py found flicker here at resolution 100 in an earlier run;")
        print("check n_iters/resolution consistency if this happens).")


if __name__ == "__main__":
    main()
