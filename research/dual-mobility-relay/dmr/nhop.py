"""n-hop generalization of the rho=0 belief factorization (channels.py,
beliefgrid2d.py) and the always-warm clamp theorem (switching_curves.py),
for a single serially-composed relay path with n >= 2 hops, each hop still
observed at rho=0 (full independence across hops).

Scope, precisely (see THRESHOLD_PROOF.md section on the n-hop generalization
for the full derivation and citations):

- The belief factorization (Proposition 1's n-hop analogue) generalizes
  cleanly and is proven the same way as the n=2 case: `T(0) = T1 (x) ... (x)
  Tn` (Kronecker product) and the decomposed likelihood factors n-ways, so a
  product prior stays a product under predict + decomposed-observation Bayes
  update. "rho=0" here means full independence across all n hops -- the
  comonotone-copula correlation knob generalizes to n hops via
  `min(u_1,...,u_n)` (still a valid copula, still exactly marginal-
  preserving), but that single knob cannot express *heterogeneous* pairwise
  correlation (hop 1-2 correlated, hop 3 independent, say); if that's ever
  needed the honest model is a hidden common-environment modulator, not this
  one-parameter family (flagged in the original 2026-07-17 formalization
  review as the "more physically honest" alternative, never built).
- The always-warm clamp identity/monotone-threshold theorem generalizes for
  a **binary routing choice** (path A vs. the n-hop relay B) with an
  n-dimensional belief `(beta_1,...,beta_n)` -- the clamp algebra never
  refers to dimension, and the monotonicity induction (THRESHOLD_PROOF.md's
  corrected Proposition P2 proof) goes through per-coordinate unchanged.
  This is verified here for n=3.
- This does NOT generalize to 3+ *routing alternatives* (e.g. choosing among
  3+ candidate relay vehicles) -- `min` over 3+ branches does not reduce to
  a single scalar field the way a 2-action `min` does, and that case is
  restless-bandit territory (switching costs are known to generally break
  Whittle indexability there -- Jun 2004; Glazebrook, Ruiz-Hernandez &
  Kirkbride 2006 for indexable special cases) rather than a direct extension
  of this module.
- The "warm" action modeled here (as in the n=2 model) is all-or-nothing:
  probing means observing every hop's loss bit at once. Physically, a
  relay node partway along the chain (e.g. the car, at hop 2 of a
  drone-car-satellite chain) could probe *its own* onward hop without
  needing the upstream hop's participation -- per-segment probing is not
  modeled here and would change the warm/cold wedge structure (Direction 3
  Q5 in the planning review that prompted this module); this is an explicit
  scoping choice, not an oversight, and is called out because it is exactly
  the kind of silent modeling assumption external review has caught twice
  before in this project.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels
from .beliefgrid2d import bayes_update_scalar, obs_prob_scalar, predict_scalar
from .mdp import ACTION_A, ACTION_B

CONTEXT_A, CONTEXT_B = ACTION_A, ACTION_B


def joint_transition_matrix_nhop(hops: list[channels.HopParams]) -> np.ndarray:
    """T(rho=0) = T1 (x) T2 (x) ... (x) Tn for n independent hops."""
    t = hops[0].transition_matrix()
    for hop in hops[1:]:
        t = np.kron(t, hop.transition_matrix())
    return t


def product_belief_nhop(betas: list[np.ndarray]) -> np.ndarray:
    """Joint belief over 2^n states as the n-way outer product of per-hop
    scalars, flattened in the same big-endian bit order as
    `joint_transition_matrix_nhop`'s Kronecker product (hop 1 most
    significant). Each `betas[i]` may be a scalar or an array (batched)."""
    b0 = np.asarray(betas[0])
    joint = np.stack([1.0 - b0, b0], axis=-1)  # (..., 2)
    for b in betas[1:]:
        bi = np.asarray(b)
        per_hop = np.stack([1.0 - bi, bi], axis=-1)  # (..., 2)
        joint = joint[..., :, None] * per_hop[..., None, :]
        joint = joint.reshape(*joint.shape[:-2], -1)
    return joint


def decomposed_obs_likelihood_nhop(hops: list[channels.HopParams]) -> np.ndarray:
    """P(o_decomp=(l_1,...,l_n) | state) as a (2^n, 2^n) matrix, factoring
    n-ways: state index and obs index both use the same big-endian
    (hop-1-most-significant) bit convention as `joint_transition_matrix_nhop`."""
    n = len(hops)
    n_states = 2**n
    lik = np.ones((n_states, n_states))
    for i, hop in enumerate(hops):
        shift = n - 1 - i
        s_bit = (np.arange(n_states)[:, None] >> shift) & 1
        o_bit = (np.arange(n_states)[None, :] >> shift) & 1
        e = hop.loss_prob(s_bit)
        lik *= np.where(o_bit == 1, e, 1.0 - e)
    return lik


def verify_factorization_nhop(
    hops: list[channels.HopParams], n_steps: int = 5, seed: int = 0
) -> float:
    """Verify a product prior stays product under repeated predict + a
    random decomposed-observation Bayes update, at rho=0, for n hops.
    Returns the max abs error between the true joint update and the
    per-hop-factored update, over `n_steps` random observation sequences."""
    n = len(hops)
    rng = np.random.default_rng(seed)
    t = joint_transition_matrix_nhop(hops)
    lik = decomposed_obs_likelihood_nhop(hops)

    betas = [np.array(0.5) for _ in hops]
    b_joint = product_belief_nhop(betas).reshape(-1)

    max_err = 0.0
    for _ in range(n_steps):
        b_pred = b_joint @ t
        betas_pred = [predict_scalar(b, hop) for b, hop in zip(betas, hops)]
        outer_pred = product_belief_nhop(betas_pred).reshape(-1)
        max_err = max(max_err, float(np.max(np.abs(b_pred - outer_pred))))

        o = rng.integers(0, 2**n)
        unnorm = b_pred * lik[:, o]
        b_post = unnorm / unnorm.sum()

        bits = [(o >> (n - 1 - i)) & 1 for i in range(n)]
        betas_post = [
            bayes_update_scalar(bp, hop, l) for bp, hop, l in zip(betas_pred, hops, bits)
        ]
        outer_post = product_belief_nhop(betas_post).reshape(-1)
        max_err = max(max_err, float(np.max(np.abs(b_post - outer_post))))

        b_joint, betas = b_post, betas_post
    return max_err


class RegularGridND:
    """A regular `(resolution+1)^n` grid over `[0,1]^n` with vectorized
    multilinear interpolation -- the n-hop analogue of
    `beliefgrid2d.RegularGrid2D`. Only used here for small n (n=3 in the
    verification below); cost scales as `resolution^n`, so this is not
    meant to replace the 2D solver for the n=2 production path."""

    def __init__(self, n_dims: int, resolution: int):
        self.n_dims = n_dims
        self.resolution = resolution
        self.axis = np.linspace(0.0, 1.0, resolution + 1)
        mesh = np.meshgrid(*([self.axis] * n_dims), indexing="ij")
        self.betas = [m.reshape(-1) for m in mesh]
        self.n_points = self.betas[0].shape[0]
        self.shape = tuple([resolution + 1] * n_dims)

    def joint_probs(self) -> np.ndarray:
        return product_belief_nhop(self.betas)

    def interpolate_batch(self, values_flat: np.ndarray, query_betas: list[np.ndarray]) -> np.ndarray:
        r = self.resolution
        values_grid = values_flat.reshape(self.shape)
        idx_lo = []
        frac = []
        for q in query_betas:
            qc = np.clip(q, 0.0, 1.0) * r
            lo = np.clip(np.floor(qc).astype(int), 0, r - 1)
            idx_lo.append(lo)
            frac.append(qc - lo)
        n = self.n_dims
        out = np.zeros_like(frac[0])
        for corner in range(2**n):
            weight = np.ones_like(frac[0])
            idx = []
            for i in range(n):
                bit = (corner >> i) & 1
                weight = weight * (frac[i] if bit else (1.0 - frac[i]))
                idx.append(idx_lo[i] + bit)
            out = out + weight * values_grid[tuple(idx)]
        return out


@dataclass(frozen=True)
class AlwaysWarmSolutionNHop:
    grid: RegularGridND
    hops: list
    cost_a: float
    c_warm: float
    c_switch_warm: float
    h: np.ndarray
    d: np.ndarray
    g: float


def _continuation_always_warm_nhop(grid: RegularGridND, hops, h_slice: np.ndarray) -> np.ndarray:
    """n-hop analogue of switching_curves._continuation_always_warm:
    unconditionally observable (standby forced warm), branch over all 2^n
    observation combinations."""
    n = len(hops)
    cont = np.zeros(grid.n_points)
    for o in range(2**n):
        bits = [(o >> (n - 1 - i)) & 1 for i in range(n)]
        p = np.ones(grid.n_points)
        next_betas = []
        for beta, hop, l in zip(grid.betas, hops, bits):
            p = p * obs_prob_scalar(beta, hop, l)
            next_betas.append(predict_scalar(bayes_update_scalar(beta, hop, l), hop))
        cont += p * grid.interpolate_batch(h_slice, next_betas)
    return cont


def always_warm_value_iteration_nhop(
    hops: list[channels.HopParams],
    cost_a: float,
    c_warm: float,
    c_switch_warm: float,
    resolution: int = 20,
    ref_point: float = 0.5,
    ref_context: int = CONTEXT_A,
    n_iters: int = 2000,
    tol: float = 1e-9,
) -> AlwaysWarmSolutionNHop:
    """n-hop analogue of switching_curves.always_warm_value_iteration:
    2 routing actions (A, the n-hop relay B), standby forced WARM every
    step, context = currently-active path. See module docstring for the
    exact scope (binary routing choice only)."""
    n = len(hops)
    grid = RegularGridND(n, resolution)
    path_loss = 1.0
    jp = grid.joint_probs()
    e_list = [hop.loss_prob(np.array([0, 1])) for hop in hops]
    # path loss at each grid point: 1 - prod_i (1 - e_i(s_i)), computed via
    # the same joint-probability weighting used for content_b below
    state_loss = np.ones(2**n)
    for i in range(n):
        shift = n - 1 - i
        bit = (np.arange(2**n) >> shift) & 1
        e = np.where(bit == 1, e_list[i][1], e_list[i][0])
        state_loss = state_loss * (1.0 - e)
    state_loss = 1.0 - state_loss

    content_a = np.full(grid.n_points, cost_a + c_warm)
    content_b = jp @ state_loss + c_warm

    ref_index = int(np.argmin(sum((b - ref_point) ** 2 for b in grid.betas)))

    h = np.zeros((grid.n_points, 2))
    g = 0.0
    for _ in range(n_iters):
        cont_a = _continuation_always_warm_nhop(grid, hops, h[:, CONTEXT_A])
        cont_b = _continuation_always_warm_nhop(grid, hops, h[:, CONTEXT_B])
        base_a = content_a + cont_a
        base_b = content_b + cont_b
        q_a = np.stack([base_a, base_b + c_switch_warm], axis=1)
        q_b = np.stack([base_a + c_switch_warm, base_b], axis=1)
        h_full = np.stack([q_a.min(axis=1), q_b.min(axis=1)], axis=1)
        g_new = float(h_full[ref_index, ref_context])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    cont_a = _continuation_always_warm_nhop(grid, hops, h[:, CONTEXT_A])
    cont_b = _continuation_always_warm_nhop(grid, hops, h[:, CONTEXT_B])
    base_a = content_a + cont_a
    base_b = content_b + cont_b
    d = base_b - base_a

    return AlwaysWarmSolutionNHop(
        grid=grid, hops=hops, cost_a=cost_a, c_warm=c_warm, c_switch_warm=c_switch_warm,
        h=h, d=d, g=g,
    )


def check_monotone_nd(field_flat: np.ndarray, grid: RegularGridND, tol: float = 1e-9) -> dict:
    """n-dimensional analogue of switching_curves.check_monotone_grid:
    checks nondecreasing-ness along every axis independently."""
    field = field_flat.reshape(grid.shape)
    result = {}
    for axis in range(grid.n_dims):
        diff = np.diff(field, axis=axis)
        viol = diff < -tol
        result[f"n_violations_axis{axis}"] = int(viol.sum())
        result[f"max_violation_axis{axis}"] = float(-diff[viol].min()) if np.any(viol) else 0.0
    return result


__all__ = [
    "joint_transition_matrix_nhop",
    "product_belief_nhop",
    "decomposed_obs_likelihood_nhop",
    "verify_factorization_nhop",
    "RegularGridND",
    "AlwaysWarmSolutionNHop",
    "always_warm_value_iteration_nhop",
    "check_monotone_nd",
]
