"""Stage 0 single-scenario demo: sanity-check every piece end to end.

Run with: uv run python run_stage0.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, changepoint, information, mdp, policy_eval, switching


def run_scenario(
    label: str,
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    rho: float,
    cost_a: float,
    gamma: float,
    c_switch: float,
    n_traj: int,
    n_steps: int,
    burn_in: int,
) -> None:
    print(f"\n{'=' * 70}\nscenario: {label}\n{'=' * 70}")
    print(f"hop1 stationary P(bad)={hop1.stationary_bad_prob():.3f}, "
          f"mean bad burst={hop1.mean_bad_burst_length():.1f} steps, eps_bad={hop1.eps_bad}")
    print(f"hop2 stationary P(bad)={hop2.stationary_bad_prob():.3f}, "
          f"mean bad burst={hop2.mean_bad_burst_length():.1f} steps, eps_bad={hop2.eps_bad}")

    t = channels.joint_transition_matrix(hop1, hop2, rho)
    stationary = channels.stationary_distribution(t)
    print("joint stationary distribution over {GG,GB,BG,BB}:", np.round(stationary, 4))

    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = mdp.cost_matrix(path_b_loss, cost_a)
    solution = mdp.value_iteration(t, cost, gamma=gamma)
    print("full-info optimal policy per state (0=A, 1=B), no switching cost:", solution.policy)

    comp_lik = channels.composite_obs_likelihood(hop1, hop2)
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)

    mi_composite = information.mutual_information(stationary, comp_lik)
    mi_decomp = information.mutual_information(stationary, decomp_lik)
    print(f"I(X; O_composite) = {mi_composite:.4f} bits, "
          f"I(X; O_decomp) = {mi_decomp:.4f} bits, "
          f"gain = {mi_decomp - mi_composite:.4f} bits")

    always_b_cost_nosw = policy_eval.fixed_action_average_cost(t, cost, mdp.ACTION_B)
    oracle_cost_nosw = policy_eval.oracle_average_cost(t, cost)
    n_small = max(50, n_traj // 4)
    result_composite = policy_eval.simulate_belief_policy(
        t, comp_lik, cost, solution, n_small, n_steps, burn_in, seed=1
    )
    result_decomp = policy_eval.simulate_belief_policy(
        t, decomp_lik, cost, solution, n_small, n_steps, burn_in, seed=2
    )
    gain = result_composite.mean_cost - result_decomp.mean_cost
    print(f"[no switching cost] policy value gain from decomposition: {gain:.4f} "
          f"(max possible vs oracle: {always_b_cost_nosw - oracle_cost_nosw:.4f})")

    # --- switching-cost-augmented model ---
    switch_cost = switching.cost_with_switching(path_b_loss, cost_a, c_switch)
    switch_solution = switching.average_cost_value_iteration_switch(t, switch_cost)
    print(f"\n[switching cost={c_switch}] policy(channel_state, active_path) -> action:")
    for c in range(4):
        print(f"  state={channels.STATE_LABELS[c]}: "
              f"if active=A -> {'B' if switch_solution.policy[c, 0] == 1 else 'A'}, "
              f"if active=B -> {'B' if switch_solution.policy[c, 1] == 1 else 'A'}")

    switch_oracle_cost = switching.induced_chain_avg_cost(t, switch_cost, switch_solution.policy)
    switch_always_a_cost = switching.induced_chain_avg_cost(
        t, switch_cost, switching.constant_policy(4, mdp.ACTION_A)
    )
    switch_always_b_cost = switching.induced_chain_avg_cost(
        t, switch_cost, switching.constant_policy(4, mdp.ACTION_B)
    )
    print(f"[switching cost] oracle average cost: {switch_oracle_cost:.4f}, "
          f"always-A: {switch_always_a_cost:.4f}, always-B: {switch_always_b_cost:.4f}")

    switch_result_composite = switching.simulate_belief_policy_switch(
        t, comp_lik, switch_cost, switch_solution, n_traj, n_steps, burn_in, seed=3
    )
    switch_result_decomp = switching.simulate_belief_policy_switch(
        t, decomp_lik, switch_cost, switch_solution, n_traj, n_steps, burn_in, seed=4
    )
    print(f"[switching cost] composite-observation belief policy: "
          f"{switch_result_composite.mean_cost:.4f} +/- {switch_result_composite.stderr_cost:.4f}")
    print(f"[switching cost] hop-decomposed belief policy:        "
          f"{switch_result_decomp.mean_cost:.4f} +/- {switch_result_decomp.stderr_cost:.4f}")
    switch_gain = switch_result_composite.mean_cost - switch_result_decomp.mean_cost
    switch_max_possible_gain = min(switch_always_a_cost, switch_always_b_cost) - switch_oracle_cost
    print(f"[switching cost] policy value gain from decomposition: {switch_gain:.4f} "
          f"(max possible vs oracle: {switch_max_possible_gain:.4f}, "
          f"{'' if switch_max_possible_gain <= 1e-9 else f'{100 * switch_gain / switch_max_possible_gain:.1f}% of ceiling'})")

    # --- CUSUM hop-attribution demo on one sample trajectory ---
    rng = np.random.default_rng(42)
    n_demo_steps = 400
    states = np.zeros(n_demo_steps, dtype=int)
    state = int(rng.choice(4, p=stationary))
    for i in range(n_demo_steps):
        state = int(rng.choice(4, p=t[state]))
        states[i] = state
    s1, s2 = channels.state_hop_indices(states)
    l1 = rng.random(n_demo_steps) < hop1.loss_prob(s1)
    l2 = rng.random(n_demo_steps) < hop2.loss_prob(s2)

    _, alarms1 = changepoint.cusum_hop_detector(l1, hop1.eps_good, hop1.eps_bad, threshold=5.0)
    _, alarms2 = changepoint.cusum_hop_detector(l2, hop2.eps_good, hop2.eps_bad, threshold=5.0)
    print(f"\nCUSUM demo over {n_demo_steps} steps: "
          f"hop1 alarms={alarms1.sum()}, hop2 alarms={alarms2.sum()} "
          f"(true bad-step counts: hop1={int((s1 == channels.BAD).sum())}, "
          f"hop2={int((s2 == channels.BAD).sum())})")


def main() -> None:
    cost_a = 0.05
    rho = 0.2
    gamma = 0.95
    n_traj, n_steps, burn_in = 300, 1200, 200

    # Scenario 1: hop1 bad state is nearly as catastrophic as hop2's. Under
    # any plausible switching cost, bailing beats riding out even a short
    # hop1 outage, because the per-step loss while Bad already dominates.
    run_scenario(
        "catastrophic hop1 (loss ~40% while Bad) -- pessimistic for decomposition",
        hop1=channels.HopParams(p_gb=0.05, p_bg=0.3, eps_good=0.01, eps_bad=0.4),
        hop2=channels.HopParams(p_gb=0.02, p_bg=0.1, eps_good=0.01, eps_bad=0.5),
        rho=rho, cost_a=cost_a, gamma=gamma, c_switch=0.15,
        n_traj=n_traj, n_steps=n_steps, burn_in=burn_in,
    )

    # Scenario 2: hop1 bad state is a brief, moderate degradation (e.g. a
    # short UHF/Wi-Fi obstruction that costs some retransmits, not near-total
    # loss), while hop2 bad is long and severe (WAN gateway effectively down).
    # This is the regime the plan's hypothesis actually needs.
    run_scenario(
        "brief moderate hop1 vs. long severe hop2 -- favorable for decomposition",
        hop1=channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12),
        hop2=channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6),
        rho=rho, cost_a=0.08, gamma=gamma, c_switch=0.1,
        n_traj=n_traj, n_steps=n_steps, burn_in=burn_in,
    )


if __name__ == "__main__":
    main()
