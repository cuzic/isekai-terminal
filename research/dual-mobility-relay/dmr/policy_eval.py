"""QMDP belief-based routing policy and Monte Carlo policy evaluation.

Because path choice does not affect how the hidden channel states evolve
(see mdp.py), Q*(x, a) - Q*(x, b) = cost(x, a) - cost(x, b) for any state x:
the future-value term is identical across actions. So the belief-weighted
"QMDP" action rule below (argmin_a E_belief[Q*(x,a)]) is *exactly* the
Bayes-optimal action under partial observability for this reward structure,
not merely an approximation -- the only thing worth approximating away
(the belief simplex's continuous value function) never enters the decision.
This isolates the question Stage 0 cares about: how much does resolving
which hop is degraded improve the instantaneous routing decision, setting
aside any credit-assignment-across-time effects a full POMDP solver would
otherwise need to handle (e.g. "wait, hop1 might recover" -- a *future*
extension in which actions could influence transitions).
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels, filtering
from .mdp import ACTION_A, MDPSolution


@dataclass
class PolicyValueResult:
    mean_cost: float
    stderr_cost: float
    per_trajectory_cost: np.ndarray


def qmdp_action(belief: np.ndarray, q: np.ndarray) -> int:
    expected_q = belief @ q  # (n_actions,)
    return int(np.argmin(expected_q))


def _sample_next_state(rng: np.random.Generator, state: int, t: np.ndarray) -> int:
    return int(rng.choice(4, p=t[state]))


def _sample_obs(rng: np.random.Generator, state: int, obs_likelihood: np.ndarray) -> int:
    n_obs = obs_likelihood.shape[1]
    return int(rng.choice(n_obs, p=obs_likelihood[state]))


def simulate_belief_policy(
    t: np.ndarray,
    obs_likelihood: np.ndarray,
    cost: np.ndarray,
    solution: MDPSolution,
    n_traj: int,
    n_steps: int,
    burn_in: int,
    seed: int,
) -> PolicyValueResult:
    """Average per-step cost of the belief-based (QMDP) policy under `obs_likelihood`.

    `cost` may differ in interpretation from what's baked into `solution.q`
    only if you intentionally want a mismatched Q (not used here); normally
    pass the same cost matrix used to build `solution`.
    """
    rng = np.random.default_rng(seed)
    stationary_start = channels.stationary_distribution(t)
    costs = np.zeros(n_traj)

    for i in range(n_traj):
        state = int(rng.choice(4, p=stationary_start))
        belief = filtering.initial_belief()
        total_cost = 0.0
        n_counted = 0
        for step in range(n_steps):
            belief_pred = filtering.predict(belief, t)
            action = qmdp_action(belief_pred, solution.q)
            state = _sample_next_state(rng, state, t)
            step_cost = cost[state, action]
            obs = _sample_obs(rng, state, obs_likelihood)
            belief = filtering.update(belief_pred, obs_likelihood[:, obs])
            if step >= burn_in:
                total_cost += step_cost
                n_counted += 1
        costs[i] = total_cost / n_counted

    return PolicyValueResult(
        mean_cost=float(costs.mean()),
        stderr_cost=float(costs.std(ddof=1) / np.sqrt(n_traj)),
        per_trajectory_cost=costs,
    )


def oracle_average_cost(t: np.ndarray, cost: np.ndarray) -> float:
    """Exact average cost of the full-state-observed (myopic-optimal) policy."""
    stationary = channels.stationary_distribution(t)
    best_cost_per_state = cost.min(axis=1)
    return float(stationary @ best_cost_per_state)


def fixed_action_average_cost(t: np.ndarray, cost: np.ndarray, action: int) -> float:
    """Exact average cost of a naive always-use-one-path baseline."""
    stationary = channels.stationary_distribution(t)
    return float(stationary @ cost[:, action])


__all__ = [
    "PolicyValueResult",
    "qmdp_action",
    "simulate_belief_policy",
    "oracle_average_cost",
    "fixed_action_average_cost",
    "ACTION_A",
]
