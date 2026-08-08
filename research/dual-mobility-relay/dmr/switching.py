"""Routing MDP augmented with a switching cost (hysteresis).

The plain 2-action model in mdp.py has a blind spot: since routing choice
doesn't affect the channel's future evolution, the myopic-optimal action at
any state is just argmin_a cost(state, a) -- and if path B's loss under
*either* hop being Bad already exceeds path A's fixed loss, the optimal
action is "switch to A" regardless of *which* hop is Bad. In that regime
hop decomposition has zero decision value even though it has positive
mutual information, because the two single-hop-bad states already share
the same optimal action.

The plan's actual hypothesis (section 4.2) is that hop2 (car<->WAN)
degradation should trigger an immediate switch, while hop1 (drone<->car)
degradation -- being shorter and potentially self-resolving as the vehicle
repositions -- is worth riding out. That asymmetry only has real
consequences if switching carries a cost (renegotiation/path-validation
overhead, a brief disruption): then bailing on a short-lived hop1 outage
that would have self-resolved before the next decision point wastes a
switch (and possibly a switch back), while bailing on a long-lived hop2
outage pays that cost once and is worth it.

This module augments the state with the currently-active path (known
exactly by the controller, not hidden) and adds a switching cost to the
per-step reward, making the transition action-dependent (choosing action a
deterministically sets next-step's active path to a) and the resulting
Bellman backup non-myopic.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels, filtering
from .mdp import ACTION_A, ACTION_B


@dataclass
class SwitchPolicyValueResult:
    mean_cost: float
    stderr_cost: float
    per_trajectory_cost: np.ndarray


@dataclass(frozen=True)
class SwitchSolution:
    q: np.ndarray  # (n_channel_states, 2, 2): Q*(channel_state, active, action)
    v: np.ndarray  # (n_channel_states, 2): V*(channel_state, active)
    policy: np.ndarray  # (n_channel_states, 2): argmin_a Q*


@dataclass(frozen=True)
class SwitchAvgSolution:
    q: np.ndarray  # (n_channel_states, 2, 2): relative Q(channel_state, active, action)
    h: np.ndarray  # (n_channel_states, 2): relative (bias) value, h[ref] == 0
    g: float  # long-run average cost of the optimal policy
    policy: np.ndarray  # (n_channel_states, 2): argmin_a Q


def cost_with_switching(path_b_loss: np.ndarray, cost_a: float, c_switch: float) -> np.ndarray:
    """cost[c, active, a] = per-step routing loss(c, a) + switch penalty."""
    n_c = len(path_b_loss)
    cost = np.zeros((n_c, 2, 2))
    cost[:, :, ACTION_A] = cost_a
    cost[:, :, ACTION_B] = path_b_loss[:, None]
    for active in range(2):
        for a in range(2):
            if a != active:
                cost[:, active, a] += c_switch
    return cost


def value_iteration_switch(
    t_channel: np.ndarray,
    cost: np.ndarray,
    gamma: float = 0.95,
    n_iters: int = 1000,
    tol: float = 1e-12,
) -> SwitchSolution:
    n_c = t_channel.shape[0]
    v = np.zeros((n_c, 2))
    for _ in range(n_iters):
        ev = t_channel @ v  # ev[c, a] = sum_c' t_channel[c, c'] * v[c', a]
        q = cost + gamma * ev[:, None, :]
        v_new = q.min(axis=2)
        if np.max(np.abs(v_new - v)) < tol:
            v = v_new
            break
        v = v_new
    ev = t_channel @ v
    q = cost + gamma * ev[:, None, :]
    policy = np.argmin(q, axis=2)
    return SwitchSolution(q=q, v=v, policy=policy)


def average_cost_value_iteration_switch(
    t_channel: np.ndarray,
    cost: np.ndarray,
    ref_state: tuple[int, int] = (0, 0),
    n_iters: int = 20000,
    tol: float = 1e-11,
) -> SwitchAvgSolution:
    """Relative value iteration (RVI) for the long-run *average*-cost
    criterion, replacing `value_iteration_switch`'s discounted (gamma)
    optimization. All of this module's evaluators (`induced_chain_avg_cost`,
    the Monte Carlo simulators) already score long-run average cost, so
    planning for a discounted objective was a criterion mismatch flagged in
    an external formalization review (Fable, 2026-07-17): the discounted-
    optimal policy and its hysteresis-band location depend on an arbitrary
    gamma, and need not be average-cost optimal.

    Standard RVI for a unichain average-cost MDP: h(s) + g = min_a[cost(s,a)
    + E[h(s')]], with h pinned at `ref_state` (subtracted each iteration) so
    it stays bounded instead of drifting; g converges to the optimal
    long-run average cost.
    """
    n_c = t_channel.shape[0]
    h = np.zeros((n_c, 2))
    g = 0.0
    ref_c, ref_p = ref_state
    for _ in range(n_iters):
        ev = t_channel @ h  # ev[c, a] = sum_c' t_channel[c, c'] * h[c', a]
        q = cost + ev[:, None, :]
        h_full = q.min(axis=2)
        g_new = float(h_full[ref_c, ref_p])
        h_new = h_full - g_new
        if np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol:
            h, g = h_new, g_new
            break
        h, g = h_new, g_new
    ev = t_channel @ h
    q = cost + ev[:, None, :]
    policy = np.argmin(q, axis=2)
    return SwitchAvgSolution(q=q, h=h, g=g, policy=policy)


def qmdp_action(belief_over_channel: np.ndarray, active: int, q: np.ndarray) -> int:
    expected_q = belief_over_channel @ q[:, active, :]
    return int(np.argmin(expected_q))


def induced_chain_avg_cost(t_channel: np.ndarray, cost: np.ndarray, policy: np.ndarray) -> float:
    """Exact long-run average cost of a deterministic policy(c, active) -> a.

    Builds the induced Markov chain over (channel_state, active_path)
    (still small: 4 x 2 = 8 states) and reads off the stationary average
    cost -- exact, no Monte Carlo needed, since active' = policy(c, active)
    deterministically and c evolves independently of the action.
    """
    n_c = t_channel.shape[0]
    n_s = n_c * 2
    t_induced = np.zeros((n_s, n_s))
    flat_cost = np.zeros(n_s)
    for c in range(n_c):
        for active in range(2):
            src = c * 2 + active
            a = int(policy[c, active])
            flat_cost[src] = cost[c, active, a]
            for c_next in range(n_c):
                dst = c_next * 2 + a
                t_induced[src, dst] += t_channel[c, c_next]
    stationary = channels.stationary_distribution(t_induced)
    return float(stationary @ flat_cost)


def constant_policy(n_c: int, action: int) -> np.ndarray:
    return np.full((n_c, 2), action, dtype=int)


def simulate_belief_policy_switch(
    t_channel: np.ndarray,
    obs_likelihood: np.ndarray,
    cost: np.ndarray,
    solution: SwitchSolution,
    n_traj: int,
    n_steps: int,
    burn_in: int,
    seed: int,
    initial_active: int = ACTION_B,
) -> SwitchPolicyValueResult:
    """Average per-step cost (including switch penalties) of the belief-based
    policy under `obs_likelihood`, tracking the currently-active path
    (known exactly -- it's the controller's own last decision) alongside
    the hidden channel-state belief.

    Hop1/hop2 losses are only observable while path B actually carries
    traffic this step (action == ACTION_B) -- parked on path A, there is no
    live signal to measure, so belief just predicts forward with no
    correction. An earlier version drew an observation unconditionally
    every step regardless of the routing action, which is physically wrong
    and was flagged in an external formalization review (Codex + Fable,
    2026-07-17).

    Vectorized across all `n_traj` trajectories at once (the Python loop is
    only over `n_steps`) -- a per-trajectory Python loop here is orders of
    magnitude slower for the grid sizes the parameter sweep needs.
    """
    rng = np.random.default_rng(seed)
    n_c = t_channel.shape[0]
    n_obs = obs_likelihood.shape[1]
    stationary_start = channels.stationary_distribution(t_channel)

    c = _sample_categorical_rows(rng, np.tile(stationary_start, (n_traj, 1)))
    active = np.full(n_traj, initial_active, dtype=int)
    belief = np.tile(filtering.initial_belief(), (n_traj, 1))
    total_cost = np.zeros(n_traj)
    n_counted = 0

    for step in range(n_steps):
        c = _sample_categorical_rows(rng, t_channel[c])
        belief_pred = belief @ t_channel

        q_active = solution.q[:, active, :]  # (n_c, n_traj, 2)
        expected_q = np.einsum("tc,cta->ta", belief_pred, q_active)
        action = np.argmin(expected_q, axis=1)

        step_cost = cost[c, active, action]

        observable = action == ACTION_B
        obs = _sample_categorical_rows(rng, obs_likelihood[c])
        lik_col = obs_likelihood[:, obs].T  # (n_traj, n_c)
        unnorm = belief_pred * lik_col
        updated = unnorm / unnorm.sum(axis=1, keepdims=True)
        belief = np.where(observable[:, None], updated, belief_pred)

        active = action
        if step >= burn_in:
            total_cost += step_cost
            n_counted += 1

    costs = total_cost / n_counted
    return SwitchPolicyValueResult(
        mean_cost=float(costs.mean()),
        stderr_cost=float(costs.std(ddof=1) / np.sqrt(n_traj)),
        per_trajectory_cost=costs,
    )


def _sample_categorical_rows(rng: np.random.Generator, row_probs: np.ndarray) -> np.ndarray:
    """Vectorized categorical sampling: one draw per row of `row_probs`."""
    cumprobs = np.cumsum(row_probs, axis=1)
    u = rng.random(row_probs.shape[0])
    idx = np.sum(cumprobs <= u[:, None], axis=1)
    return np.clip(idx, 0, row_probs.shape[1] - 1)


__all__ = [
    "SwitchSolution",
    "SwitchAvgSolution",
    "SwitchPolicyValueResult",
    "cost_with_switching",
    "value_iteration_switch",
    "average_cost_value_iteration_switch",
    "qmdp_action",
    "induced_chain_avg_cost",
    "constant_policy",
    "simulate_belief_policy_switch",
    "ACTION_A",
    "ACTION_B",
]
