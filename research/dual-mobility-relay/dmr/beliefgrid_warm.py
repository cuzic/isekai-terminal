"""Exact (up to grid resolution) belief-grid POMDP solve for the
warm-standby model, replacing QMDP.

Belief-MDP formulation: state = (belief b over the 4 channel states,
active path p, standby warm status w). `b` here always means the
*decision-time* (post-transition, pre-observation) belief -- i.e. exactly
`belief_pred` in `warm_standby.simulate_belief_policy_warm` -- so the
recursion below mirrors that simulator's causal structure exactly:

    h(b, p, w) + g = min_{k=(a,m)} [ b @ cost[:, p, w, k]
                                    + E_o[ h(b_next(b, a, m, o), a, m) ] ]

where, if the action is observable ((a==B) or (m==WARM)):
    b_next(b,a,m,o) = predict(update(b, o), T_channel)
        with P(o|b) = b @ obs_likelihood[:, o]
and if not observable, there is no o and b_next = predict(b, T_channel).

This is the average-cost (RVI) criterion, matching
`switching.average_cost_value_iteration_switch` /
`warm_standby.average_cost_value_iteration_warm` for consistency (see the
2026-07-17 formalization review on criterion mismatch). h is solved by
value iteration over the belief grid, with the continuation term looked up
via `BeliefGrid.interpolate_batch`.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import beliefgrid
from .mdp import ACTION_B
from .warm_standby import ACTIONS, WARM


@dataclass(frozen=True)
class BeliefGridWarmSolution:
    grid: beliefgrid.BeliefGrid
    h: np.ndarray  # (n_points, 2, 2): relative value at each grid belief x (p, w)
    g: float
    q: np.ndarray  # (n_points, 2, 2, 4): relative Q at each grid belief x (p, w) x action


@dataclass(frozen=True)
class FixedPolicyEvalSolution:
    grid: beliefgrid.BeliefGrid
    h: np.ndarray  # (n_points, 2, 2): relative value of the fixed policy
    g: float  # long-run average cost of the fixed policy, exact up to grid resolution


def _continuation(
    grid: beliefgrid.BeliefGrid,
    t_channel: np.ndarray,
    obs_likelihood: np.ndarray,
    h: np.ndarray,
    a: int,
    m: int,
) -> np.ndarray:
    """E[h(b_next, a, m) | belief_i, action=(a,m)] for every grid point i at once."""
    beliefs = grid.beliefs  # (n_points, n_c)
    h_slice = h[:, a, m]  # (n_points,)
    observable = (a == ACTION_B) or (m == WARM)

    if not observable:
        b_next = beliefs @ t_channel
        return grid.interpolate_batch(h_slice, b_next)

    n_points = beliefs.shape[0]
    n_obs = obs_likelihood.shape[1]
    cont = np.zeros(n_points)
    for o in range(n_obs):
        p_o = beliefs @ obs_likelihood[:, o]  # (n_points,)
        safe_p_o = np.where(p_o > 1e-12, p_o, 1.0)
        b_post = (beliefs * obs_likelihood[:, o]) / safe_p_o[:, None]
        b_next = b_post @ t_channel
        interp_vals = grid.interpolate_batch(h_slice, b_next)
        cont += np.where(p_o > 1e-12, p_o * interp_vals, 0.0)
    return cont


def belief_grid_value_iteration_warm(
    t_channel: np.ndarray,
    cost: np.ndarray,
    obs_likelihood: np.ndarray,
    resolution: int = 16,
    ref_grid_index: int | None = None,
    ref_context: tuple[int, int] = (0, 0),
    n_iters: int = 500,
    tol: float = 1e-8,
) -> BeliefGridWarmSolution:
    n_c = t_channel.shape[0]
    grid = beliefgrid.BeliefGrid(n_c, resolution)
    n_points = len(grid.points)
    if ref_grid_index is None:
        # use a near-uniform belief as the RVI reference point
        uniform = np.full(n_c, 1.0 / n_c)
        ref_grid_index = int(np.argmin(np.sum((grid.beliefs - uniform) ** 2, axis=1)))
    ref_p, ref_w = ref_context

    h = np.zeros((n_points, 2, 2))
    g = 0.0
    immediate = np.stack(
        [
            [grid.beliefs @ cost[:, p, w, k] for k in range(4)]
            for p in range(2)
            for w in range(2)
        ]
    ).reshape(2, 2, 4, n_points)  # [p, w, k, point]

    for _ in range(n_iters):
        cont_by_action = np.stack(
            [
                _continuation(grid, t_channel, obs_likelihood, h, *ACTIONS[k])
                for k in range(4)
            ]
        )  # (4, n_points)

        q = (immediate + cont_by_action[None, None, :, :]).transpose(0, 1, 3, 2)
        # q shape: (p, w, point, k)
        h_full = q.min(axis=3)  # (2, 2, n_points)
        h_full = h_full.transpose(2, 0, 1)  # (n_points, 2, 2)

        g_new = float(h_full[ref_grid_index, ref_p, ref_w])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    cont_by_action = np.stack(
        [_continuation(grid, t_channel, obs_likelihood, h, *ACTIONS[k]) for k in range(4)]
    )
    q = (immediate + cont_by_action[None, None, :, :]).transpose(0, 1, 3, 2)
    q = q.transpose(2, 0, 1, 3)  # (n_points, 2, 2, 4)
    return BeliefGridWarmSolution(grid=grid, h=h, g=g, q=q)


def belief_grid_action(solution: BeliefGridWarmSolution, belief: np.ndarray, p: int, w: int) -> int:
    """Argmin action index at an arbitrary (interpolated) belief point."""
    expected_q = np.array(
        [solution.grid.interpolate(solution.q[:, p, w, k], belief) for k in range(4)]
    )
    return int(np.argmin(expected_q))


def evaluate_fixed_policy_belief_grid_warm(
    t_channel: np.ndarray,
    cost: np.ndarray,
    obs_likelihood: np.ndarray,
    policy_fn,
    resolution: int = 16,
    ref_grid_index: int | None = None,
    ref_context: tuple[int, int] = (0, 0),
    n_iters: int = 500,
    tol: float = 1e-8,
) -> FixedPolicyEvalSolution:
    """Exact (up to grid resolution), noise-free policy *evaluation* (not
    optimization) on the belief-simplex grid: reuses `_continuation`'s
    branching machinery but plugs in a fixed policy's chosen action at
    each grid point instead of minimizing over actions.

    `policy_fn(beliefs, p, w)` must return an `(n_points,)` int array of
    action indices (0..3, indexing `warm_standby.ACTIONS`) for that
    context, given `beliefs` = `(n_points, n_c)` grid points.

    Used to evaluate a policy derived from a *different* model (e.g. the
    rho=0 2D belief-MDP's optimal policy) against this solver's own
    (possibly rho>0) `t_channel`/`obs_likelihood`, without Monte Carlo
    noise -- e.g. to check how a rho=0-designed policy performs in a
    rho>0 world, which a Monte Carlo rollout can only estimate with
    sampling error that may swamp a small true effect.
    """
    n_c = t_channel.shape[0]
    grid = beliefgrid.BeliefGrid(n_c, resolution)
    n_points = len(grid.points)
    if ref_grid_index is None:
        uniform = np.full(n_c, 1.0 / n_c)
        ref_grid_index = int(np.argmin(np.sum((grid.beliefs - uniform) ** 2, axis=1)))
    ref_p, ref_w = ref_context

    policy = np.zeros((n_points, 2, 2), dtype=int)
    for p in range(2):
        for w in range(2):
            policy[:, p, w] = policy_fn(grid.beliefs, p, w)

    idx_pts = np.arange(n_points)
    immediate_all = np.stack(
        [
            [grid.beliefs @ cost[:, p, w, k] for k in range(4)]
            for p in range(2)
            for w in range(2)
        ]
    ).reshape(2, 2, 4, n_points)  # [p, w, k, point]
    immediate = np.zeros((n_points, 2, 2))
    for p in range(2):
        for w in range(2):
            immediate[:, p, w] = immediate_all[p, w, policy[:, p, w], idx_pts]

    h = np.zeros((n_points, 2, 2))
    g = 0.0
    for _ in range(n_iters):
        cont_by_action = np.stack(
            [_continuation(grid, t_channel, obs_likelihood, h, *ACTIONS[k]) for k in range(4)]
        )  # (4, n_points)
        h_full = np.zeros((n_points, 2, 2))
        for p in range(2):
            for w in range(2):
                h_full[:, p, w] = immediate[:, p, w] + cont_by_action[policy[:, p, w], idx_pts]

        g_new = float(h_full[ref_grid_index, ref_p, ref_w])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    return FixedPolicyEvalSolution(grid=grid, h=h, g=g)


__all__ = [
    "BeliefGridWarmSolution",
    "FixedPolicyEvalSolution",
    "belief_grid_value_iteration_warm",
    "belief_grid_action",
    "evaluate_fixed_policy_belief_grid_warm",
]
