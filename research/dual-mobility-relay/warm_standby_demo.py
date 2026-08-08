"""Adaptive warm-standby demo: when is it worth paying c_warm?

Part 1 compares three FULLY-OBSERVED policies (the channel state c is known
exactly -- the oracle setting), all with routing action `a` optimized:
- fully adaptive: also chooses whether to keep the standby path warm (m),
  conditioned on the channel state.
- always-warm: m is forced to WARM every step (the naive "just always keep
  the backup hot" baseline) -- pays c_warm constantly, never pays the
  expensive cold-switch cost.
- always-cold: m is forced to COLD every step (the naive "never bother"
  baseline) -- pays zero standby-maintenance cost, but always pays the
  expensive cold-switch cost on any failover.

Part 2 switches to PARTIAL observability (belief-based QMDP), where hop1/hop2
losses are only observable while path B carries traffic or the standby is
warm (dmr/warm_standby.py's `simulate_belief_policy_warm`, fixed 2026-07-17
per an external formalization review). This is the setting where hop
decomposition can actually matter: `switching.py`'s belief-tracking model
(no warm-standby option) was shown to get *permanently* stuck on path A once
it bails, because with no way to observe path B ever again there's no
information -- composite or decomposed -- to justify switching back. Here,
choosing to warm the standby is what buys fresh observations, so composite
vs decomposed observation can differ in how quickly/confidently the
controller decides to re-engage path B.

Run with: uv run python warm_standby_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, warm_standby


def main() -> None:
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
    rho = 0.2
    cost_a = 0.08

    c_warm = 0.01          # per-step battery/data premium for keeping standby warm
    c_switch_warm = 0.02   # cheap failover: standby was already validated
    c_switch_cold = 0.3    # expensive failover: full handshake/path-validation from scratch

    t = channels.joint_transition_matrix(hop1, hop2, rho)
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, cost_a, c_warm, c_switch_warm, c_switch_cold
    )
    solution = warm_standby.average_cost_value_iteration_warm(t, cost)

    print(f"c_warm={c_warm}, c_switch_warm={c_switch_warm}, c_switch_cold={c_switch_cold}\n")
    print("optimal policy(channel_state, active_path, standby_warm?) -> (route, maintain):")
    for c in range(4):
        for p in range(2):
            for w in range(2):
                k = int(solution.policy[c, p, w])
                print(f"  state={channels.STATE_LABELS[c]:>2} active={'A' if p==0 else 'B'} "
                      f"standby_currently={'warm' if w else 'cold'}: -> {warm_standby.ACTION_LABELS[k]}")

    adaptive_cost = warm_standby.induced_chain_avg_cost(t, cost, solution.policy)
    always_warm_policy = warm_standby.constrained_policy(solution.q, fixed_m=warm_standby.WARM)
    always_cold_policy = warm_standby.constrained_policy(solution.q, fixed_m=warm_standby.COLD)
    always_warm_cost = warm_standby.induced_chain_avg_cost(t, cost, always_warm_policy)
    always_cold_cost = warm_standby.induced_chain_avg_cost(t, cost, always_cold_policy)

    print(f"\nexact long-run average cost:")
    print(f"  fully adaptive:  {adaptive_cost:.5f}")
    print(f"  always warm:     {always_warm_cost:.5f}")
    print(f"  always cold:     {always_cold_cost:.5f}")
    print(f"\nadaptive vs always-warm: saves {always_warm_cost - adaptive_cost:.5f} "
          f"({100*(always_warm_cost - adaptive_cost)/always_warm_cost:.1f}% of always-warm's cost)")
    print(f"adaptive vs always-cold: saves {always_cold_cost - adaptive_cost:.5f} "
          f"({100*(always_cold_cost - adaptive_cost)/always_cold_cost:.1f}% of always-cold's cost)")

    # how much of the adaptive policy's steady-state time is actually spent warm?
    stationary_c = channels.stationary_distribution(t)
    # approximate: assume p tracks the routing-optimal path, check warm-fraction
    # of the policy table directly (unconditional on visitation frequency) as a
    # quick summary of "how often does it choose to warm at all".
    warm_choices = sum(
        1
        for c in range(4)
        for p in range(2)
        for w in range(2)
        if warm_standby.ACTIONS[int(solution.policy[c, p, w])][1] == warm_standby.WARM
    )
    print(f"\nfraction of (state,active,standby) combinations where adaptive policy chooses "
          f"to warm: {warm_choices}/16")

    # --- Part 2: partial observability -- does hop decomposition matter now? ---
    print("\n--- partial observability (belief-based QMDP) ---")
    comp_lik = channels.composite_obs_likelihood(hop1, hop2)
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)
    n_traj, n_steps, burn_in = 400, 1500, 300

    result_composite = warm_standby.simulate_belief_policy_warm(
        t, comp_lik, cost, solution, n_traj, n_steps, burn_in, seed=10
    )
    result_decomp = warm_standby.simulate_belief_policy_warm(
        t, decomp_lik, cost, solution, n_traj, n_steps, burn_in, seed=11
    )
    print(f"composite-observation belief policy: "
          f"{result_composite.mean_cost:.5f} +/- {result_composite.stderr_cost:.5f}")
    print(f"hop-decomposed belief policy:        "
          f"{result_decomp.mean_cost:.5f} +/- {result_decomp.stderr_cost:.5f}")
    gain = result_composite.mean_cost - result_decomp.mean_cost
    stderr_gain = float(np.sqrt(result_composite.stderr_cost**2 + result_decomp.stderr_cost**2))
    print(f"policy value gain from decomposition: {gain:.5f} +/- {stderr_gain:.5f}")
    print(f"(for reference, fully-observed/oracle adaptive cost was {adaptive_cost:.5f} -- "
          f"belief-based cost should sit between the oracle and the fixed-m baselines)")


if __name__ == "__main__":
    main()
