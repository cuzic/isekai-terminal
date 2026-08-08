"""Exact (up to grid resolution) belief-MDP solve exploiting the rho=0
product-belief reduction, replacing the general 4-state simplex solve
(`beliefgrid_warm.py`) with a much cheaper/higher-resolution 2D solve.

Why this reduction is valid (see FORMALIZATION_REVIEW.md's follow-up review,
2026-07-18, Codex + an independent Fable-model agent, both consulted before
writing this module):

At `rho=0`, `channels.joint_transition_matrix(hop1, hop2, 0)` is exactly
`kron(T1, T2)` and `channels.decomposed_obs_likelihood` factors as
`p1(l1|s1) * p2(l2|s2)`. A standard Kronecker identity
(`(T1(x)T2)(b1(x)b2) = (T1 b1)(x)(T2 b2)`) then shows that a *product* prior
`b1 (x) b2` stays a product after `predict`, and a short Bayes-rule
computation shows it also stays a product after a decomposed-observation
update (the joint unnormalized posterior factors into
`[b1_pred*p1(l1|.)] (x) [b2_pred*p2(l2|.)]`, and the normalizer itself splits
as `Z1*Z2`). So the sufficient statistic collapses from a 4-state joint
belief (a 3-simplex) to two independent scalars
`beta1 = P(hop1=Bad)`, `beta2 = P(hop2=Bad)`.

This does NOT hold for composite observation (`FORMALIZATION_REVIEW.md`'s
follow-up review works out that the loss-likelihood matrix
`M[s1,s2] = 1 - (1-e1(s1))(1-e2(s2))` has `det M = -(e1_bad-e1_good) *
(e2_bad-e2_good)`, nonzero whenever both hops are actually informative --
i.e. entanglement is unavoidable exactly when hop identity carries any
decision-relevant information). The composite baseline must keep using the
general `beliefgrid_warm.py` solve; this module is rho=0/decomposed-obs
only, and does not apply to composite observation or rho>0.

Two caveats carried over from the review (both purely about the bilinear-
interpolation lower-bound argument used by `beliefgrid_warm.py`/Lovejoy 1991
for the simplex grid, which does not transfer to this module unmodified):
the POMDP value function is concave in the *joint* 4-state belief, but the
embedding `(beta1,beta2) -> b1 (x) b2` is bilinear, not affine, so
`h(beta1,beta2)` is not automatically jointly concave in `(beta1,beta2)`.
What *is* guaranteed is separate concavity (the embedding is affine in each
coordinate holding the other fixed), which is enough for bilinear
interpolation to still underestimate: interpolating in beta1 first uses a
chord that underestimates by concavity in beta1 alone, then interpolating
that underestimate in beta2 uses a chord of underestimates, itself an
underestimate. Also, the grid points here store *decision-time* (post-
transition, pre-observation) belief, exactly matching `beliefgrid_warm.py`'s
convention -- validation against it (see `beliefgrid2d_demo.py`) is only
apples-to-apples because of that.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels, filtering
from .mdp import ACTION_A, ACTION_B
from .warm_standby import ACTIONS, COLD, WARM, WarmPolicyValueResult

_P_NEXT = np.array([a for a, m in ACTIONS])
_W_NEXT = np.array([m for a, m in ACTIONS])


def predict_scalar(beta: np.ndarray, hop: channels.HopParams) -> np.ndarray:
    """beta' = P(next=Bad), pushing a per-hop marginal belief through its own
    transition row. Valid for every rho (the marginal transition row is
    preserved by construction -- see channels.py), not just rho=0."""
    return beta * (1.0 - hop.p_bg) + (1.0 - beta) * hop.p_gb


def obs_prob_scalar(beta: np.ndarray, hop: channels.HopParams, loss: int) -> np.ndarray:
    """P(loss | beta) marginal over a single hop's belief."""
    p_bad = hop.eps_bad if loss else (1.0 - hop.eps_bad)
    p_good = hop.eps_good if loss else (1.0 - hop.eps_good)
    return beta * p_bad + (1.0 - beta) * p_good


def bayes_update_scalar(beta: np.ndarray, hop: channels.HopParams, loss: int) -> np.ndarray:
    """Posterior P(Bad | loss, prior beta) for a single hop's 2-state HMM.

    FIXED 2026-07-19: for a pure-Gilbert channel (eps_good=0 or eps_bad=1
    exactly -- a genuine, physically legitimate parameterization, e.g. a
    real GE moment-method fit from an actual binary loss/success trace, not
    just a synthetic corner case), `total` can be EXACTLY 0 at a belief-grid
    boundary point (beta=0 or beta=1) for the branch of `loss` that has zero
    probability there. The old `unnorm_bad / total` produced NaN (0/0) in
    that branch; even though every caller immediately multiplies this by
    that same branch's own probability `obs_prob_scalar(...)` (which is also
    exactly 0 there), `NaN * 0 == NaN`, not `0` -- so the NaN silently
    propagated through the entire RVI fixed point instead of being zeroed
    out, corrupting every downstream result (caught via a real-trace
    calibration run using an actual eps_good=0/eps_bad=1 fit -- see
    TRACE_CALIBRATION_NOTES.md). Since this zero-probability branch is never
    actually reached by the physical process, any finite value here is
    correct once multiplied by its own zero probability -- `where=total>0`
    substitutes 0 there instead of computing 0/0."""
    p_bad = hop.eps_bad if loss else (1.0 - hop.eps_bad)
    p_good = hop.eps_good if loss else (1.0 - hop.eps_good)
    beta = np.asarray(beta, dtype=float)
    unnorm_bad = beta * p_bad
    unnorm_good = (1.0 - beta) * p_good
    total = unnorm_bad + unnorm_good
    return np.divide(unnorm_bad, total, out=np.zeros_like(beta), where=total > 0)


def product_belief(beta1: np.ndarray, beta2: np.ndarray) -> np.ndarray:
    """Joint 4-state belief `[P(GG), P(GB), P(BG), P(BB)]` (or batched, shape
    `(..., 4)`) implied by independent per-hop scalars, matching the
    `s1*2+s2` state-index convention used throughout `channels.py`."""
    beta1 = np.asarray(beta1)
    beta2 = np.asarray(beta2)
    return np.stack(
        [
            (1.0 - beta1) * (1.0 - beta2),
            (1.0 - beta1) * beta2,
            beta1 * (1.0 - beta2),
            beta1 * beta2,
        ],
        axis=-1,
    )


class RegularGrid2D:
    """A regular `(resolution+1) x (resolution+1)` grid over `[0,1]^2` with
    vectorized bilinear interpolation -- the 2D analogue of
    `beliefgrid.BeliefGrid`'s Delaunay-based simplex grid, much cheaper
    because the domain is a square, not a triangulated simplex."""

    def __init__(self, resolution: int):
        self.resolution = resolution
        self.axis = np.linspace(0.0, 1.0, resolution + 1)
        b1_mesh, b2_mesh = np.meshgrid(self.axis, self.axis, indexing="ij")
        self.beta1 = b1_mesh.reshape(-1)
        self.beta2 = b2_mesh.reshape(-1)
        self.n_points = self.beta1.shape[0]
        self.shape = (resolution + 1, resolution + 1)

    def joint_probs(self) -> np.ndarray:
        """`(n_points, 4)` joint-state probabilities at every grid point."""
        return product_belief(self.beta1, self.beta2)

    def interpolate_batch(
        self, values_flat: np.ndarray, b1_query: np.ndarray, b2_query: np.ndarray
    ) -> np.ndarray:
        """Bilinear interpolation of a function known at all grid points
        (`values_flat`, indexed like `self.beta1`/`self.beta2`) at arbitrary
        query points, vectorized over the query batch."""
        r = self.resolution
        values_grid = values_flat.reshape(self.shape)
        b1c = np.clip(b1_query, 0.0, 1.0)
        b2c = np.clip(b2_query, 0.0, 1.0)
        f1 = b1c * r
        f2 = b2c * r
        i1 = np.clip(np.floor(f1).astype(int), 0, r - 1)
        i2 = np.clip(np.floor(f2).astype(int), 0, r - 1)
        t1 = f1 - i1
        t2 = f2 - i2
        v00 = values_grid[i1, i2]
        v10 = values_grid[i1 + 1, i2]
        v01 = values_grid[i1, i2 + 1]
        v11 = values_grid[i1 + 1, i2 + 1]
        return v00 * (1 - t1) * (1 - t2) + v10 * t1 * (1 - t2) + v01 * (1 - t1) * t2 + v11 * t1 * t2


@dataclass(frozen=True)
class BeliefGrid2DWarmSolution:
    grid: RegularGrid2D
    hop1: channels.HopParams
    hop2: channels.HopParams
    h: np.ndarray  # (n_points, 2, 2): relative value at each grid point x (p, w)
    g: float
    q: np.ndarray  # (n_points, 2, 2, 4): relative Q at each grid point x (p, w) x action


def _continuation(
    grid: RegularGrid2D,
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    h: np.ndarray,
    a: int,
    m: int,
) -> np.ndarray:
    """E[h(next belief, a, m) | (beta1,beta2), action=(a,m)] at every grid
    point at once. Mirrors `beliefgrid_warm._continuation`'s decision-time-
    belief convention exactly, but branches over the 4 `(l1,l2)` combinations
    using independent per-hop Bayes updates instead of the joint 4-state
    update."""
    b1, b2 = grid.beta1, grid.beta2
    h_slice = h[:, a, m]
    observable = (a == ACTION_B) or (m == WARM)

    if not observable:
        b1_next = predict_scalar(b1, hop1)
        b2_next = predict_scalar(b2, hop2)
        return grid.interpolate_batch(h_slice, b1_next, b2_next)

    cont = np.zeros(grid.n_points)
    for l1 in (0, 1):
        p1 = obs_prob_scalar(b1, hop1, l1)
        b1_next = predict_scalar(bayes_update_scalar(b1, hop1, l1), hop1)
        for l2 in (0, 1):
            p2 = obs_prob_scalar(b2, hop2, l2)
            b2_next = predict_scalar(bayes_update_scalar(b2, hop2, l2), hop2)
            interp_vals = grid.interpolate_batch(h_slice, b1_next, b2_next)
            cont += p1 * p2 * interp_vals
    return cont


def belief_grid2d_value_iteration_warm(
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    cost: np.ndarray,
    resolution: int = 100,
    ref_point: tuple[float, float] = (0.5, 0.5),
    ref_context: tuple[int, int] = (0, 0),
    n_iters: int = 2000,
    tol: float = 1e-9,
) -> BeliefGrid2DWarmSolution:
    """RVI (average-cost) value iteration over `h(beta1, beta2, p, w)`,
    rho=0 / decomposed-observation only. `cost` is the same
    `(n_c=4, 2, 2, 4)` array `warm_standby.cost_with_warm_standby` produces
    (indexed by the 4-state joint channel, not by `(beta1,beta2)` -- the
    immediate cost at a grid point is `product_belief(beta1,beta2) @
    cost[:, p, w, k]`, i.e. exactly the `beliefs @ cost` pattern
    `beliefgrid_warm.py` uses, with `product_belief` standing in for a
    general joint belief since it's always product-form here)."""
    grid = RegularGrid2D(resolution)
    jp = grid.joint_probs()  # (n_points, 4)
    immediate = np.stack(
        [
            [jp @ cost[:, p, w, k] for k in range(4)]
            for p in range(2)
            for w in range(2)
        ]
    ).reshape(2, 2, 4, grid.n_points)  # [p, w, k, point]

    ref_b1, ref_b2 = ref_point
    ref_index = int(np.argmin((grid.beta1 - ref_b1) ** 2 + (grid.beta2 - ref_b2) ** 2))
    ref_p, ref_w = ref_context

    h = np.zeros((grid.n_points, 2, 2))
    g = 0.0
    for _ in range(n_iters):
        cont_by_action = np.stack(
            [_continuation(grid, hop1, hop2, h, *ACTIONS[k]) for k in range(4)]
        )  # (4, n_points)

        q = (immediate + cont_by_action[None, None, :, :]).transpose(0, 1, 3, 2)
        # q shape: (p, w, point, k)
        h_full = q.min(axis=3)  # (2, 2, n_points)
        h_full = h_full.transpose(2, 0, 1)  # (n_points, 2, 2)

        g_new = float(h_full[ref_index, ref_p, ref_w])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    cont_by_action = np.stack(
        [_continuation(grid, hop1, hop2, h, *ACTIONS[k]) for k in range(4)]
    )
    q = (immediate + cont_by_action[None, None, :, :]).transpose(0, 1, 3, 2)
    q = q.transpose(2, 0, 1, 3)  # (n_points, 2, 2, 4)
    return BeliefGrid2DWarmSolution(grid=grid, hop1=hop1, hop2=hop2, h=h, g=g, q=q)


def belief_grid2d_action(
    solution: BeliefGrid2DWarmSolution, beta1: float, beta2: float, p: int, w: int
) -> int:
    """Argmin action index at an arbitrary (interpolated) belief point."""
    b1 = np.array([beta1])
    b2 = np.array([beta2])
    expected_q = np.array(
        [
            solution.grid.interpolate_batch(solution.q[:, p, w, k], b1, b2)[0]
            for k in range(4)
        ]
    )
    return int(np.argmin(expected_q))


def simulate_belief_policy_2d(
    t_channel: np.ndarray,
    obs_likelihood: np.ndarray,
    cost: np.ndarray,
    solution: BeliefGrid2DWarmSolution,
    n_traj: int,
    n_steps: int,
    burn_in: int,
    seed: int,
    initial_p: int = ACTION_B,
    initial_w: int = COLD,
) -> WarmPolicyValueResult:
    """Monte Carlo validation of the 2D-grid policy: tracks the actual
    general 4-state joint belief (via the exact HMM filter, not artificially
    forced into product form) and the true joint channel simulation, and
    only *projects* the belief onto its `(beta1, beta2)` marginals to look up
    the 2D-grid solution's action. This is the fair independent check that
    claim 1's exact factorization (verified separately, see
    FORMALIZATION_REVIEW.md's 2026-07-18 follow-up) actually holds along
    realized trajectories, not just for one predict/update step: if it did
    not, this simulation's cost would systematically diverge from
    `solution.g`, since the joint filter here is authoritative and the 2D
    lookup is only ever an approximation of it.

    Valid at rho=0 with `obs_likelihood` = `channels.decomposed_obs_likelihood`
    only (mirrors `belief_grid2d_value_iteration_warm`'s scope)."""
    rng = np.random.default_rng(seed)
    n_c = t_channel.shape[0]
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
        beta1_pred = belief_pred[:, 2] + belief_pred[:, 3]
        beta2_pred = belief_pred[:, 1] + belief_pred[:, 3]

        expected_q = np.zeros((n_traj, 4))
        for pp in range(2):
            for ww in range(2):
                mask = (p == pp) & (w == ww)
                if not np.any(mask):
                    continue
                for k in range(4):
                    expected_q[mask, k] = solution.grid.interpolate_batch(
                        solution.q[:, pp, ww, k], beta1_pred[mask], beta2_pred[mask]
                    )
        action_idx = np.argmin(expected_q, axis=1)
        a = _P_NEXT[action_idx]
        m = _W_NEXT[action_idx]

        step_cost = cost[c, p, w, action_idx]

        observable = (a == ACTION_B) | (m == WARM)
        obs = _sample_categorical_rows(rng, obs_likelihood[c])
        lik_col = obs_likelihood[:, obs].T
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


__all__ = [
    "RegularGrid2D",
    "BeliefGrid2DWarmSolution",
    "predict_scalar",
    "obs_prob_scalar",
    "bayes_update_scalar",
    "product_belief",
    "belief_grid2d_value_iteration_warm",
    "belief_grid2d_action",
    "simulate_belief_policy_2d",
]
