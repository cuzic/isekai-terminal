"""Validation of the rho=0 (beta1, beta2) belief-MDP reduction
(dmr/beliefgrid2d.py) against the general 4-state simplex solve
(dmr/beliefgrid_warm.py) and against a Monte Carlo rollout that tracks the
true joint belief (dmr/beliefgrid2d.py::simulate_belief_policy_2d).

Run with: uv run python beliefgrid2d_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, beliefgrid_warm, channels, warm_standby


def main() -> None:
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
    rho = 0.0  # the 2D reduction is rho=0 only
    cost_a = 0.08
    c_warm, c_switch_warm, c_switch_cold = 0.01, 0.02, 0.3

    t = channels.joint_transition_matrix(hop1, hop2, rho)
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, cost_a, c_warm, c_switch_warm, c_switch_cold
    )
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)

    print("=== Cross-check 1: 2D solve vs. general 4-state simplex solve, rho=0 ===")
    sol_simplex = beliefgrid_warm.belief_grid_value_iteration_warm(
        t, cost, decomp_lik, resolution=16, n_iters=400
    )
    print(f"simplex solve (resolution=16, n_points={len(sol_simplex.grid.points)}): "
          f"g={sol_simplex.g:.6f}")

    for resolution in [20, 40, 60, 100]:
        sol_2d = beliefgrid2d.belief_grid2d_value_iteration_warm(
            hop1, hop2, cost, resolution=resolution, n_iters=2000
        )
        print(f"2D solve      (resolution={resolution:>3}, n_points={sol_2d.grid.n_points:>5}): "
              f"g={sol_2d.g:.6f}")

    print(
        "\n(both are lower bounds via linear/bilinear interpolation on a concave-in-belief\n"
        "value function -- expect convergence toward each other from below as resolution\n"
        "grows in both, not exact equality at finite resolution; the 2D solve should reach\n"
        "a tighter (higher) g at far less compute since its grid is 2D, not a 3-simplex.)"
    )

    print("\n=== Cross-check 2: single-fixed-action sanity check ===")
    # Force an extreme parameter regime where path A is always strictly
    # better than path B regardless of channel state, so the optimal policy
    # is "always route A, never warm" from every belief -- letting us check
    # the 2D solve's g against a closed-form exact answer that doesn't
    # involve belief tracking at all (since the induced (p,w) chain becomes
    # absorbing at (A,COLD) after one step regardless of initial (p,w)).
    cost_a_extreme = 0.5  # worse than the realistic scenario but still tiny vs. path B here
    path_b_loss_bad = np.full(4, 0.9)  # path B always terrible
    cost_extreme = warm_standby.cost_with_warm_standby(
        path_b_loss_bad, cost_a_extreme, c_warm, c_switch_warm, c_switch_cold
    )
    stationary_c = channels.stationary_distribution(t)
    # action k=0 is (A, COLD); once absorbed there, switch cost is paid at
    # most once (amortized to zero over the infinite horizon average), so
    # the exact average cost is just cost_a_extreme.
    exact_g_extreme = cost_a_extreme
    sol_2d_extreme = beliefgrid2d.belief_grid2d_value_iteration_warm(
        hop1, hop2, cost_extreme, resolution=40, n_iters=2000
    )
    print(f"exact g (always-A-cold is absorbing-optimal): {exact_g_extreme:.6f}")
    print(f"2D solve g:                                    {sol_2d_extreme.g:.6f}")
    print(f"difference: {abs(sol_2d_extreme.g - exact_g_extreme):.2e} (should be tiny)")

    print("\n=== Cross-check 3: Monte Carlo rollout tracking the TRUE joint belief ===")
    resolution = 80
    sol_2d = beliefgrid2d.belief_grid2d_value_iteration_warm(
        hop1, hop2, cost, resolution=resolution, n_iters=2000
    )
    print(f"2D solve (resolution={resolution}): g={sol_2d.g:.6f}")
    mc_result = beliefgrid2d.simulate_belief_policy_2d(
        t, decomp_lik, cost, sol_2d, n_traj=400, n_steps=1500, burn_in=300, seed=42
    )
    print(f"MC rollout (true joint belief/filter, action via 2D-grid lookup): "
          f"{mc_result.mean_cost:.6f} +/- {mc_result.stderr_cost:.6f}")
    z = abs(mc_result.mean_cost - sol_2d.g) / mc_result.stderr_cost
    print(f"|MC - RVI g| / stderr = {z:.2f} standard errors "
          f"(should be small -- large would mean the belief factorization broke\n"
          f"along real trajectories, or the 2D policy is suboptimal against the true filter)")


if __name__ == "__main__":
    main()
