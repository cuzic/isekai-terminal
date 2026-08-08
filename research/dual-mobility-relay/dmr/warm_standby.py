"""Adaptive warm-standby: whether to keep the *inactive* path pre-validated.

Extends switching.py's (channel_state, active_path) MDP with a third
dimension: the warm/cold status of the currently-inactive path. Warming has
a one-step lag -- choosing "warm" this step makes the standby usable at
cheap switch cost *next* step, not this one, so it must be a persistent
state, not just a same-step action.

Action is now a pair (a, m):
  a: which path carries traffic this step (as in switching.py).
  m: whether to keep maintaining the *other* (currently-inactive) path warm
     going forward.

cost(c, p, w, a, m) = routing_loss(c, a)
                     + c_warm * 1{m == WARM}                 -- paid every
                       step you choose to warm, regardless of whether you
                       ever fail over (the "battery/data" premium).
                     + switch_cost(w) * 1{a != p}             -- paid only
                       if you actually fail over this step; cheap if the
                       destination was already warm, expensive (full
                       handshake/path-validation) if it was cold.

The 4 (a, m) combinations are encoded as a single action index 0..3:
  0: (A, cold)   1: (A, warm)   2: (B, cold)   3: (B, warm)
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels, filtering
from .mdp import ACTION_A, ACTION_B

COLD, WARM = 0, 1

ACTIONS = [(ACTION_A, COLD), (ACTION_A, WARM), (ACTION_B, COLD), (ACTION_B, WARM)]
ACTION_LABELS = ["(A,cold)", "(A,warm)", "(B,cold)", "(B,warm)"]
_P_NEXT = np.array([a for a, m in ACTIONS])
_W_NEXT = np.array([m for a, m in ACTIONS])


@dataclass(frozen=True)
class WarmSolution:
    q: np.ndarray  # (n_c, 2, 2, 4): Q*(c, p, w, action_index)
    v: np.ndarray  # (n_c, 2, 2)
    policy: np.ndarray  # (n_c, 2, 2) -> action index 0..3


@dataclass
class WarmPolicyValueResult:
    mean_cost: float
    stderr_cost: float
    per_trajectory_cost: np.ndarray


@dataclass(frozen=True)
class WarmAvgSolution:
    q: np.ndarray  # (n_c, 2, 2, 4): relative Q(c, p, w, action_index)
    h: np.ndarray  # (n_c, 2, 2): relative (bias) value, h[ref] == 0
    g: float  # long-run average cost of the optimal policy
    policy: np.ndarray  # (n_c, 2, 2) -> action index 0..3


def cost_with_warm_standby(
    path_b_loss: np.ndarray,
    cost_a: float,
    c_warm: float,
    c_switch_warm: float,
    c_switch_cold: float,
) -> np.ndarray:
    """cost[c, p, w, k] for k indexing ACTIONS."""
    n_c = len(path_b_loss)
    cost = np.zeros((n_c, 2, 2, 4))
    for k, (a, m) in enumerate(ACTIONS):
        route_loss = cost_a if a == ACTION_A else path_b_loss  # scalar or (n_c,)
        warm_cost = c_warm if m == WARM else 0.0
        for p in range(2):
            for w in range(2):
                sc = 0.0 if a == p else (c_switch_warm if w == WARM else c_switch_cold)
                cost[:, p, w, k] = route_loss + warm_cost + sc
    return cost


def value_iteration_warm(
    t_channel: np.ndarray,
    cost: np.ndarray,
    gamma: float = 0.95,
    n_iters: int = 1000,
    tol: float = 1e-12,
) -> WarmSolution:
    n_c = t_channel.shape[0]
    v = np.zeros((n_c, 2, 2))
    for _ in range(n_iters):
        v_at_action = v[:, _P_NEXT, _W_NEXT]  # (n_c, 4)
        ev = t_channel @ v_at_action  # (n_c, 4)
        q = cost + gamma * ev[:, None, None, :]
        v_new = q.min(axis=3)
        if np.max(np.abs(v_new - v)) < tol:
            v = v_new
            break
        v = v_new
    v_at_action = v[:, _P_NEXT, _W_NEXT]
    ev = t_channel @ v_at_action
    q = cost + gamma * ev[:, None, None, :]
    policy = np.argmin(q, axis=3)
    return WarmSolution(q=q, v=v, policy=policy)


def average_cost_value_iteration_warm(
    t_channel: np.ndarray,
    cost: np.ndarray,
    ref_state: tuple[int, int, int] = (0, 0, 0),
    n_iters: int = 20000,
    tol: float = 1e-11,
) -> WarmAvgSolution:
    """Relative value iteration (RVI) for the long-run average-cost
    criterion -- see `switching.average_cost_value_iteration_switch` for the
    full rationale (criterion-consistency fix from the 2026-07-17
    formalization review)."""
    n_c = t_channel.shape[0]
    h = np.zeros((n_c, 2, 2))
    g = 0.0
    ref_c, ref_p, ref_w = ref_state
    for _ in range(n_iters):
        h_at_action = h[:, _P_NEXT, _W_NEXT]  # (n_c, 4)
        ev = t_channel @ h_at_action  # (n_c, 4)
        q = cost + ev[:, None, None, :]
        h_full = q.min(axis=3)  # (n_c, 2, 2)
        g_new = float(h_full[ref_c, ref_p, ref_w])
        h_new = h_full - g_new
        if np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol:
            h, g = h_new, g_new
            break
        h, g = h_new, g_new
    h_at_action = h[:, _P_NEXT, _W_NEXT]
    ev = t_channel @ h_at_action
    q = cost + ev[:, None, None, :]
    policy = np.argmin(q, axis=3)
    return WarmAvgSolution(q=q, h=h, g=g, policy=policy)


def qmdp_action(belief_over_c: np.ndarray, p: int, w: int, q: np.ndarray) -> int:
    expected_q = belief_over_c @ q[:, p, w, :]
    return int(np.argmin(expected_q))


def induced_chain_avg_cost(t_channel: np.ndarray, cost: np.ndarray, policy: np.ndarray) -> float:
    """Exact long-run average cost of a deterministic policy(c, p, w) -> action index."""
    n_c = t_channel.shape[0]
    n_s = n_c * 4  # (c, p, w) flattened
    t_induced = np.zeros((n_s, n_s))
    flat_cost = np.zeros(n_s)
    for c in range(n_c):
        for p in range(2):
            for w in range(2):
                src = c * 4 + p * 2 + w
                k = int(policy[c, p, w])
                flat_cost[src] = cost[c, p, w, k]
                p_next, w_next = _P_NEXT[k], _W_NEXT[k]
                for c_next in range(n_c):
                    dst = c_next * 4 + p_next * 2 + w_next
                    t_induced[src, dst] += t_channel[c, c_next]
    stationary = channels.stationary_distribution(t_induced)
    return float(stationary @ flat_cost)


def simulate_belief_policy_warm(
    t_channel: np.ndarray,
    obs_likelihood: np.ndarray,
    cost: np.ndarray,
    solution: WarmSolution,
    n_traj: int,
    n_steps: int,
    burn_in: int,
    seed: int,
    initial_p: int = ACTION_B,
    initial_w: int = COLD,
) -> WarmPolicyValueResult:
    """Average per-step cost of the belief-based (QMDP) policy under
    `obs_likelihood`, tracking (active path `p`, standby warm status `w`)
    exactly (the controller's own past choices) alongside a belief over the
    hidden channel state.

    Hop1/hop2 losses are observable this step iff the route action `a`
    chosen this step is B (live traffic), or `a` is A and the chosen
    maintenance action `m` is WARM (a probe on the standby). Otherwise there
    is no signal and belief only predicts forward. This is what makes
    keeping the standby warm buy *information*, not just a cheaper future
    switch cost -- the gap identified in the 2026-07-17 formalization review
    (Codex + Fable) as missing from the original model.
    """
    rng = np.random.default_rng(seed)
    n_c = t_channel.shape[0]
    n_obs = obs_likelihood.shape[1]
    stationary_start = channels.stationary_distribution(t_channel)

    c = _sample_categorical_rows(rng, np.tile(stationary_start, (n_traj, 1)))
    p = np.full(n_traj, initial_p, dtype=int)
    w = np.full(n_traj, initial_w, dtype=int)
    belief = np.tile(filtering.initial_belief(), (n_traj, 1))
    total_cost = np.zeros(n_traj)
    n_counted = 0

    for step in range(n_steps):
        c = _sample_categorical_rows(rng, t_channel[c])
        belief_pred = belief @ t_channel

        q_context = solution.q[:, p, w, :]  # (n_c, n_traj, 4)
        expected_q = np.einsum("tc,ctk->tk", belief_pred, q_context)
        action_idx = np.argmin(expected_q, axis=1)
        a = _P_NEXT[action_idx]
        m = _W_NEXT[action_idx]

        step_cost = cost[c, p, w, action_idx]

        observable = (a == ACTION_B) | (m == WARM)
        obs = _sample_categorical_rows(rng, obs_likelihood[c])
        lik_col = obs_likelihood[:, obs].T  # (n_traj, n_c)
        unnorm = belief_pred * lik_col
        updated = unnorm / unnorm.sum(axis=1, keepdims=True)
        belief = np.where(observable[:, None], updated, belief_pred)

        p, w = a, m
        if step >= burn_in:
            total_cost += step_cost
            n_counted += 1

    costs = total_cost / n_counted
    return WarmPolicyValueResult(
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


def constrained_policy(q: np.ndarray, fixed_m: int | None) -> np.ndarray:
    """Optimal policy restricted to a fixed warm-maintenance choice `fixed_m`
    (COLD or WARM) every step, still optimizing routing action `a` freely.
    Pass fixed_m=None for the fully adaptive (unconstrained) policy."""
    n_c = q.shape[0]
    if fixed_m is None:
        return np.argmin(q, axis=3)
    allowed = [k for k, (a, m) in enumerate(ACTIONS) if m == fixed_m]
    policy = np.zeros((n_c, 2, 2), dtype=int)
    for c in range(n_c):
        for p in range(2):
            for w in range(2):
                sub_q = q[c, p, w, allowed]
                policy[c, p, w] = allowed[int(np.argmin(sub_q))]
    return policy


__all__ = [
    "WarmSolution",
    "WarmAvgSolution",
    "WarmPolicyValueResult",
    "COLD",
    "WARM",
    "ACTIONS",
    "ACTION_LABELS",
    "cost_with_warm_standby",
    "value_iteration_warm",
    "average_cost_value_iteration_warm",
    "qmdp_action",
    "simulate_belief_policy_warm",
    "induced_chain_avg_cost",
    "constrained_policy",
]
