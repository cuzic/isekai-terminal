"""Analytical/semi-analytical treatment of the always-warm sub-model under PURE GILBERT
channels (eps_good=0, eps_bad=1 exactly), per the user's request (2026-07-19) for a more
quantitative/closed-form answer to the warm/cold boundary question.

KEY OBSERVATION (verified against `dmr/beliefgrid2d.py`'s Bayes update): with eps_good=0,
eps_bad=1, an observation deterministically reveals the hidden state (loss => Bad for certain,
no-loss => Good for certain). Since "always warm" means BOTH hops are observed every single step
(the active one via live traffic, the standby one via the warm probe), belief collapses to an
exact point mass after the very first step and stays there forever after. This means the
continuous-belief POMDP that `switching_curves.always_warm_value_iteration` solves numerically
degenerates, in this special case, to a FINITE, FULLY-OBSERVED average-cost MDP over
(channel_state, context) in {GG,GB,BG,BB} x {A,B} -- 8 states, 2 actions -- solvable by exact
policy iteration (finite state/action space => finite convergence, no grid/RVI-truncation error
at all).

This script (a) builds and solves that finite MDP exactly via policy iteration + a direct linear
solve of the Poisson equation for the converged policy, and (b) cross-validates the result
against the existing continuous-belief solver evaluated at the eps=0/1 boundary, to confirm the
reduction is implemented correctly before trusting it as a "more analytical" answer.

Run with: uv run python pure_gilbert_finite_mdp_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

ACTION_A, ACTION_B = 0, 1
STATE_LABELS = ["GG", "GB", "BG", "BB"]


def build_finite_mdp(hop1: channels.HopParams, hop2: channels.HopParams, cost_a: float,
                      c_warm: float, c_switch_warm: float):
    """Returns (T, immediate_cost) for the 8-state (c, p) always-warm pure-Gilbert MDP.

    IMPORTANT (bug found and fixed via cross-validation against the continuous solver, see
    module docstring / TRACE-style note below): `c` here is the LAST OBSERVED joint channel
    state, NOT the current true state. The decision timing is act-then-observe within a step:
    the routing action for this step must be chosen using the PREDICTIVE distribution of the
    CURRENT (not-yet-observed) state given the last observation, i.e.
    `E_{c' ~ T_channel[c,:]}[path_b_loss(c')]`, not `path_b_loss[c]` directly (that would wrongly
    assume the current state is already known before acting, collapsing the one-step transition
    uncertainty that the belief-POMDP's predict-then-update cycle actually has). `c` itself
    remains a valid sufficient statistic (last-observed state fully determines the predictive
    distribution of the current state, by the Markov property), so the finite-MDP reduction
    still holds -- only the cost formula needed this fix, not the state space or transition
    structure.

    T: (8,8) transition matrix indexed by (c*2+p) -> (c'*2+p'), for EACH action a separately
       (so really T[a] is (8,8) for a in {0,1}).
    immediate_cost[a]: (8,) immediate cost of taking action a in each of the 8 states.
    State index = c*2 + p, c in 0..3 ({GG,GB,BG,BB} = LAST OBSERVED state), p in {0=A,1=B}.
    """
    T_channel = channels.joint_transition_matrix(hop1, hop2, rho=0.0)  # (4,4), c -> c'
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)  # (4,), deterministic 0/1 for pure Gilbert
    assert np.all((path_b_loss == 0) | (path_b_loss == 1)), "expected pure-Gilbert deterministic path_b_loss"
    predictive_path_b_loss = T_channel @ path_b_loss  # (4,): E[path_b_loss(c') | last observed c]

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
                    sp = cp * 2 + a  # next context = action just taken
                    T[a][s, sp] = T_channel[c, cp]
    return T, cost


def solve_average_cost_policy_iteration(T: dict, cost: dict, ref_state: int = 0, n_iters: int = 50):
    """Exact average-cost policy iteration for a finite MDP (Puterman ch.8-9): alternates
    (1) exact linear solve of the Poisson equation for the current policy's (g, h), pinning
    h[ref_state]=0, and (2) a greedy policy-improvement step. Terminates in finitely many
    iterations for a finite state/action space (no grid/RVI truncation error at all)."""
    n = len(next(iter(cost.values())))
    policy = np.zeros(n, dtype=int)  # start all-A

    for it in range(n_iters):
        T_pi = np.array([T[policy[s]][s] for s in range(n)])
        c_pi = np.array([cost[policy[s]][s] for s in range(n)])

        # Poisson equation: g + h = c_pi + T_pi @ h, with h[ref_state] = 0.
        # Rearranged: h - T_pi @ h + g*1 = c_pi  =>  (I - T_pi) h + g * 1_vec = c_pi
        # n equations, n+1 unknowns (h_0..h_{n-1}, g) -- add h[ref_state]=0 as the extra equation.
        A = np.zeros((n + 1, n + 1))
        b = np.zeros(n + 1)
        A[:n, :n] = np.eye(n) - T_pi
        A[:n, n] = 1.0
        b[:n] = c_pi
        A[n, ref_state] = 1.0
        b[n] = 0.0
        # A degenerate intermediate policy (e.g. "always A") can freeze `context`, making some
        # states unreachable from ref_state -- this makes (I - T_pi) singular for those states'
        # rows even with the pinning equation added (Fable-model review flagged this multichain
        # trap in advance). lstsq's minimum-norm solution keeps iterating correctly regardless;
        # only the FINAL converged g (validated against the continuous solver already) matters,
        # not the h values for a transient policy that gets replaced next iteration anyway.
        sol, *_ = np.linalg.lstsq(A, b, rcond=None)
        h, g = sol[:n], sol[n]

        # Policy improvement: greedy w.r.t. cost[a] + T[a] @ h
        q = np.stack([cost[a] + T[a] @ h for a in (ACTION_A, ACTION_B)], axis=1)  # (n,2)
        new_policy = np.argmin(q, axis=1)

        if np.array_equal(new_policy, policy) and it > 0:
            return g, h, policy, it
        policy = new_policy

    return g, h, policy, n_iters


def main() -> None:
    COST_A = 0.30
    C_WARM = 0.02
    C_SWITCH_WARM = 0.01

    print("=== Pure-Gilbert finite-MDP reduction: exact policy iteration ===\n")
    for lam in [0.3, 0.5, 0.7, 0.9]:
        pi_bad = 0.3
        p_gb = pi_bad * (1 - lam)
        p_bg = (1 - pi_bad) * (1 - lam)
        hop = channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=0.0, eps_bad=1.0)

        T, cost = build_finite_mdp(hop, hop, COST_A, C_WARM, C_SWITCH_WARM)
        g_exact, h_exact, policy, n_iter = solve_average_cost_policy_iteration(T, cost)

        # Cross-validate against the existing continuous-belief solver at this eps=0/1 boundary.
        sol_continuous = switching_curves.always_warm_value_iteration(
            hop, hop, COST_A, C_WARM, C_SWITCH_WARM, resolution=80, n_iters=3000)

        print(f"lambda={lam}: g_exact(finite MDP, {n_iter} policy-iters)={g_exact:.8f}, "
              f"g_continuous(belief POMDP, RVI)={sol_continuous.g:.8f}, "
              f"diff={abs(g_exact - sol_continuous.g):.2e}")
        policy_labels = [f"{STATE_LABELS[s//2]}|{'A' if s%2==0 else 'B'}->{'A' if policy[s]==0 else 'B'}" for s in range(8)]
        print(f"  optimal policy (state|context->action): {policy_labels}")
    print()


if __name__ == "__main__":
    main()
