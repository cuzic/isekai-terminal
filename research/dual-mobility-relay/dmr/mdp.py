"""Fully-observed MDP over the joint 4-state channel and value iteration.

Actions are which path to route the current packet over: A (direct cellular,
state-independent baseline loss `cost_a`) or B (relay via the support
vehicle, loss = path_b_loss_prob(state)). Routing choice does not influence
how the hidden channel states evolve (choosing A vs B doesn't change the
physical radio conditions), so the transition kernel T(rho) is
action-independent. That makes the Bellman backup a plain linear fixed
point; we still iterate it explicitly (rather than solving the linear system
directly) to keep the "value iteration" framing transparent and because it
converges in a handful of iterations for a 4-state chain.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

ACTION_A, ACTION_B = 0, 1


@dataclass(frozen=True)
class MDPSolution:
    q: np.ndarray  # (4, 2): Q*(state, action)
    v: np.ndarray  # (4,): V*(state)
    policy: np.ndarray  # (4,): argmin_a Q*(state, a)
    cost: np.ndarray  # (4, 2): immediate cost(state, action)


def cost_matrix(path_b_loss: np.ndarray, cost_a: float) -> np.ndarray:
    """Immediate expected cost(state, action), shape (4, 2)."""
    cost = np.zeros((4, 2))
    cost[:, ACTION_A] = cost_a
    cost[:, ACTION_B] = path_b_loss
    return cost


def value_iteration(
    t: np.ndarray,
    cost: np.ndarray,
    gamma: float = 0.9,
    n_iters: int = 500,
    tol: float = 1e-12,
) -> MDPSolution:
    """Exact value iteration; converges quickly since T is action-independent."""
    n_states, n_actions = cost.shape
    v = np.zeros(n_states)
    for _ in range(n_iters):
        q = cost + gamma * (t @ v)[:, None]
        v_new = q.min(axis=1)
        if np.max(np.abs(v_new - v)) < tol:
            v = v_new
            break
        v = v_new
    q = cost + gamma * (t @ v)[:, None]
    policy = np.argmin(q, axis=1)
    return MDPSolution(q=q, v=v, policy=policy, cost=cost)
