"""Switching-curve extraction on the (beta1, beta2) unit square, plus the
always-warm constrained sub-model where a full monotone-threshold theorem is
provable (see the proof note this module's docstrings point to, written up
after the numerics here were verified -- FORMALIZATION_REVIEW.md's
2026-07-18 follow-up review, an independent Fable-model agent, found the
clamp identity below; Codex CLI independently confirmed the full
unconstrained model's Topkis argument fails for the reason `d_field`'s
module-level docstring explains).

Two distinct solved objects feed into curve extraction:

1. **Always-warm sub-model** (`always_warm_value_iteration`): the standby is
   forced WARM every step (`warm_standby.constrained_policy`'s regime,
   solved fresh here rather than restricting the unconstrained solve,
   because forcing warm changes observability itself -- every action is
   observable, so there is no "if observable" branch at all). This is the
   provable case: `Delta(beta) = h(beta,B) - h(beta,A)` satisfies the exact
   algebraic identity `Delta = clamp(d(beta), -c_switch_warm, c_switch_warm)`
   where `d(beta) = base(beta,B) - base(beta,A)` and `base(beta,a) =
   content(beta,a) + E_o[h(next belief, a)]` is context-independent (the
   same update kernel regardless of which context you started in, since
   observability doesn't depend on context here) -- so `d` alone, via two
   level sets at +-c_switch_warm, gives both switching curves and the exact
   hysteresis band `{|d(beta)| < c_switch_warm}`.

2. **Full unconstrained model** (`d_field_full_model`, operating on a
   `beliefgrid2d.BeliefGrid2DWarmSolution`): per fixed context `(p, w)`, the
   decision boundary is the zero level set of `min_Bish Q(.,p,w,.) -
   min_Aish Q(.,p,w,.)` directly (no +-c_switch offset needed here, since
   context-specific switch costs are already baked into `Q`). No proof
   backs monotonicity of this field in general -- see this module's
   `d_field_full_model` docstring for exactly why the always-warm proof
   doesn't transfer -- so curve extraction here reports multi-crossing
   columns instead of silently assuming a single threshold per column.

Curve extraction itself (`extract_level_curve`) uses per-column 1D linear-
interpolated root-finding, not policy-region boundary tracing or
`matplotlib.pyplot.contour`/`skimage` marching squares -- deliberately, per
the 2026-07-18 review: policy-argmax boundaries have grid-resolution
staircase artifacts, and scraping contour paths from a plotting library
makes the extraction untestable. A scalar field's level set, one root per
column, is ~15 lines of plain NumPy and is exact up to linear interpolation.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels
from .beliefgrid2d import (
    RegularGrid2D,
    bayes_update_scalar,
    obs_prob_scalar,
    predict_scalar,
)
from .beliefgrid2d import BeliefGrid2DWarmSolution
from .mdp import ACTION_A, ACTION_B
from .warm_standby import ACTIONS

CONTEXT_A, CONTEXT_B = ACTION_A, ACTION_B


@dataclass(frozen=True)
class AlwaysWarmSolution:
    grid: RegularGrid2D
    hop1: channels.HopParams
    hop2: channels.HopParams
    cost_a: float
    c_warm: float
    c_switch_warm: float
    h: np.ndarray  # (n_points, 2): relative value, h[:, context] for context in {A=0, B=1}
    base: np.ndarray  # (n_points, 2): base(beta, action) for action in {A=0, B=1}
    d: np.ndarray  # (n_points,): base[:, B] - base[:, A]
    g: float
    policy: np.ndarray  # (n_points, 2): argmin action (0=A, 1=B) in each context


def _continuation_always_warm(
    grid: RegularGrid2D, hop1: channels.HopParams, hop2: channels.HopParams, h_slice: np.ndarray
) -> np.ndarray:
    """E[h_slice(next belief)] under the always-warm sub-model, where
    observation happens unconditionally every step regardless of the action
    (path B carries live traffic; path A's standby is a probe since m is
    forced WARM) -- so there is no "if observable" branch, unlike
    `beliefgrid2d._continuation`."""
    b1, b2 = grid.beta1, grid.beta2
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


def always_warm_value_iteration(
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    cost_a: float,
    c_warm: float,
    c_switch_warm: float,
    resolution: int = 100,
    ref_point: tuple[float, float] = (0.5, 0.5),
    ref_context: int = CONTEXT_A,
    n_iters: int = 2000,
    tol: float = 1e-9,
) -> AlwaysWarmSolution:
    """RVI value iteration for the always-warm-standby sub-model: 2 routing
    actions (A, B), standby forced WARM every step, context = currently-
    active path. `content(beta,A) = cost_a + c_warm` (constant),
    `content(beta,B) = path_b_loss(beta) + c_warm`. `c_warm` cancels out of
    every action comparison (it's added identically regardless of routing
    action) but is kept in `content`/`base`/`g` for a physically meaningful
    absolute cost; it does not affect `d`, the policy, or the switching
    curves at all.
    """
    grid = RegularGrid2D(resolution)
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    content_a = np.full(grid.n_points, cost_a + c_warm)
    content_b = grid.joint_probs() @ path_b_loss + c_warm

    ref_b1, ref_b2 = ref_point
    ref_index = int(np.argmin((grid.beta1 - ref_b1) ** 2 + (grid.beta2 - ref_b2) ** 2))

    h = np.zeros((grid.n_points, 2))
    g = 0.0
    for _ in range(n_iters):
        cont_a = _continuation_always_warm(grid, hop1, hop2, h[:, CONTEXT_A])
        cont_b = _continuation_always_warm(grid, hop1, hop2, h[:, CONTEXT_B])
        base_a = content_a + cont_a
        base_b = content_b + cont_b

        q_context_a = np.stack([base_a, base_b + c_switch_warm], axis=1)
        q_context_b = np.stack([base_a + c_switch_warm, base_b], axis=1)
        h_full = np.stack([q_context_a.min(axis=1), q_context_b.min(axis=1)], axis=1)

        g_new = float(h_full[ref_index, ref_context])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    cont_a = _continuation_always_warm(grid, hop1, hop2, h[:, CONTEXT_A])
    cont_b = _continuation_always_warm(grid, hop1, hop2, h[:, CONTEXT_B])
    base_a = content_a + cont_a
    base_b = content_b + cont_b
    d = base_b - base_a
    q_context_a = np.stack([base_a, base_b + c_switch_warm], axis=1)
    q_context_b = np.stack([base_a + c_switch_warm, base_b], axis=1)
    policy = np.stack([q_context_a.argmin(axis=1), q_context_b.argmin(axis=1)], axis=1)
    base = np.stack([base_a, base_b], axis=1)

    return AlwaysWarmSolution(
        grid=grid,
        hop1=hop1,
        hop2=hop2,
        cost_a=cost_a,
        c_warm=c_warm,
        c_switch_warm=c_switch_warm,
        h=h,
        base=base,
        d=d,
        g=g,
        policy=policy,
    )


@dataclass(frozen=True)
class AlwaysColdSolution:
    grid: RegularGrid2D
    hop1: channels.HopParams
    hop2: channels.HopParams
    cost_a: float
    c_switch_cold: float
    h: np.ndarray  # (n_points, 2): relative value, h[:, context] for context in {A=0, B=1}
    g: float
    policy: np.ndarray  # (n_points, 2): argmin action (0=A, 1=B) in each context


def _continuation_always_cold(
    grid: RegularGrid2D,
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    h_slice: np.ndarray,
    observable: bool,
) -> np.ndarray:
    """E[h_slice(next belief)] under the always-cold sub-model (standby
    never warmed, matching `switching.py`'s original model exactly):
    observable iff the action being evaluated is `a=B` (live traffic);
    `a=A` never observes anything, since there is no standby to probe."""
    b1, b2 = grid.beta1, grid.beta2
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


def always_cold_value_iteration(
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    cost_a: float,
    c_switch_cold: float,
    resolution: int = 100,
    ref_point: tuple[float, float] = (0.5, 0.5),
    ref_context: int = CONTEXT_A,
    n_iters: int = 2000,
    tol: float = 1e-9,
) -> AlwaysColdSolution:
    """RVI value iteration for the always-cold sub-model: 2 routing actions
    (A, B), standby never warmed, context = currently-active path. This is
    the (beta1, beta2) belief-MDP counterpart of `switching.py`'s original
    switching-cost-only model (no warm-standby option at all) -- used here
    as the "always cold" fixed baseline against which to measure the value
    of adaptively choosing to warm."""
    grid = RegularGrid2D(resolution)
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    content_a = np.full(grid.n_points, cost_a)
    content_b = grid.joint_probs() @ path_b_loss

    ref_b1, ref_b2 = ref_point
    ref_index = int(np.argmin((grid.beta1 - ref_b1) ** 2 + (grid.beta2 - ref_b2) ** 2))

    h = np.zeros((grid.n_points, 2))
    g = 0.0
    for _ in range(n_iters):
        cont_a = _continuation_always_cold(grid, hop1, hop2, h[:, CONTEXT_A], observable=False)
        cont_b = _continuation_always_cold(grid, hop1, hop2, h[:, CONTEXT_B], observable=True)
        base_a = content_a + cont_a
        base_b = content_b + cont_b

        q_context_a = np.stack([base_a, base_b + c_switch_cold], axis=1)
        q_context_b = np.stack([base_a + c_switch_cold, base_b], axis=1)
        h_full = np.stack([q_context_a.min(axis=1), q_context_b.min(axis=1)], axis=1)

        g_new = float(h_full[ref_index, ref_context])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    cont_a = _continuation_always_cold(grid, hop1, hop2, h[:, CONTEXT_A], observable=False)
    cont_b = _continuation_always_cold(grid, hop1, hop2, h[:, CONTEXT_B], observable=True)
    base_a = content_a + cont_a
    base_b = content_b + cont_b
    q_context_a = np.stack([base_a, base_b + c_switch_cold], axis=1)
    q_context_b = np.stack([base_a + c_switch_cold, base_b], axis=1)
    policy = np.stack([q_context_a.argmin(axis=1), q_context_b.argmin(axis=1)], axis=1)

    return AlwaysColdSolution(
        grid=grid,
        hop1=hop1,
        hop2=hop2,
        cost_a=cost_a,
        c_switch_cold=c_switch_cold,
        h=h,
        g=g,
        policy=policy,
    )


def verify_clamp_identity(solution: AlwaysWarmSolution) -> float:
    """Returns `max |Delta(beta) - clamp(d(beta), -c_switch, c_switch)|`
    over the grid, where `Delta = h[:,B] - h[:,A]`. Should be ~0 (float
    precision) -- this is a pure algebraic identity from the two `min(...)`
    expressions defining `h[:,A]`/`h[:,B]`, not a claim requiring
    monotonicity; see this module's docstring."""
    delta = solution.h[:, CONTEXT_B] - solution.h[:, CONTEXT_A]
    clamped = np.clip(solution.d, -solution.c_switch_warm, solution.c_switch_warm)
    return float(np.max(np.abs(delta - clamped)))


def check_monotone_grid(field_flat: np.ndarray, grid: RegularGrid2D, tol: float = 1e-9) -> dict:
    """Checks whether `field_flat` (indexed like `grid.beta1`/`grid.beta2`)
    is nondecreasing along both the beta1 axis and the beta2 axis. Returns a
    dict with violation counts/max magnitude per axis -- used both to
    confirm the always-warm theorem's monotonicity claim (expect ~0
    violations up to float tolerance) and to numerically probe whether the
    full unconstrained model's Q-difference field happens to stay monotone
    anyway, despite having no proof that it must (gap G1)."""
    field_grid = field_flat.reshape(grid.shape)
    diff_beta1 = field_grid[1:, :] - field_grid[:-1, :]
    diff_beta2 = field_grid[:, 1:] - field_grid[:, :-1]
    viol_beta1 = diff_beta1 < -tol
    viol_beta2 = diff_beta2 < -tol
    max_viol_beta1 = float(-diff_beta1[viol_beta1].min()) if np.any(viol_beta1) else 0.0
    max_viol_beta2 = float(-diff_beta2[viol_beta2].min()) if np.any(viol_beta2) else 0.0
    return {
        "n_violations_beta1": int(viol_beta1.sum()),
        "max_violation_beta1": max_viol_beta1,
        "n_violations_beta2": int(viol_beta2.sum()),
        "max_violation_beta2": max_viol_beta2,
    }


def localize_monotonicity_violations(field_flat: np.ndarray, grid: RegularGrid2D, tol: float = 1e-9) -> dict:
    """Like `check_monotone_grid`, but returns *where* the violations are,
    not just counts/magnitude (task #45 -- `check_monotone_grid` alone
    cannot localize a counterexample to a region of the (beta1,beta2)
    square). `viol_beta1_mask`/`viol_beta2_mask` are boolean arrays over the
    adjacent-pair grid (shape one smaller than `grid.shape` along the
    differenced axis); `beta1_rows_with_beta2_violation` /
    `beta2_cols_with_beta1_violation` give the coordinate lists on the
    non-differenced axis, and the bbox fields give the tightest axis-aligned
    box (in beta1/beta2 units, not indices) containing every violation on
    that axis."""
    field_grid = field_flat.reshape(grid.shape)
    axis = grid.axis
    diff_beta1 = field_grid[1:, :] - field_grid[:-1, :]
    diff_beta2 = field_grid[:, 1:] - field_grid[:, :-1]
    viol_beta1_mask = diff_beta1 < -tol
    viol_beta2_mask = diff_beta2 < -tol

    # diff_beta1[i, j] is the beta1-direction step between rows i, i+1 at
    # fixed beta2 column j; a beta1-violation there implicates beta2 row j.
    beta2_cols_with_beta1_violation = sorted(set(int(j) for j in np.where(viol_beta1_mask.any(axis=0))[0]))
    # diff_beta2[i, j] is the beta2-direction step at fixed beta1 row i;
    # a beta2-violation there implicates beta1 row i.
    beta1_rows_with_beta2_violation = sorted(set(int(i) for i in np.where(viol_beta2_mask.any(axis=1))[0]))

    def _bbox(indices: list[int]) -> tuple[float, float] | None:
        if not indices:
            return None
        return (float(axis[min(indices)]), float(axis[max(indices)]))

    return {
        "viol_beta1_mask": viol_beta1_mask,
        "viol_beta2_mask": viol_beta2_mask,
        "beta2_cols_with_beta1_violation": beta2_cols_with_beta1_violation,
        "beta1_rows_with_beta2_violation": beta1_rows_with_beta2_violation,
        "beta2_bbox_of_beta1_violations": _bbox(beta2_cols_with_beta1_violation),
        "beta1_bbox_of_beta2_violations": _bbox(beta1_rows_with_beta2_violation),
    }


@dataclass(frozen=True)
class LevelCurve:
    beta2: np.ndarray  # column coordinates that had exactly one crossing
    beta1: np.ndarray  # interpolated beta1 at the crossing, one per beta2
    no_crossing_columns: list[int]
    multi_crossing_columns: list[tuple[int, int]]  # (column index, crossing count)


def extract_level_curve(field_flat: np.ndarray, grid: RegularGrid2D, level: float) -> LevelCurve:
    """Per-column 1D linear-interpolated root find of `field(beta1,beta2) =
    level`, holding beta2 fixed at each grid column. Reports (rather than
    silently dropping) columns with zero or more-than-one crossings -- a
    multi-crossing column would itself be a finding (non-threshold
    structure), not something to average away."""
    field_grid = field_flat.reshape(grid.shape) - level
    axis = grid.axis
    beta2_out: list[float] = []
    beta1_out: list[float] = []
    no_crossing: list[int] = []
    multi_crossing: list[tuple[int, int]] = []

    for j in range(field_grid.shape[1]):
        column = field_grid[:, j]
        signs = np.sign(column)
        crossings: list[float] = []
        for i in range(len(column) - 1):
            v0, v1 = column[i], column[i + 1]
            if v0 == 0.0:
                crossings.append(axis[i])
                continue
            if v0 * v1 < 0.0:
                frac = v0 / (v0 - v1)
                crossings.append(axis[i] + frac * (axis[i + 1] - axis[i]))
        if len(column) > 0 and column[-1] == 0.0:
            crossings.append(axis[-1])
        crossings = sorted(set(round(c, 12) for c in crossings))

        if len(crossings) == 0:
            no_crossing.append(j)
        elif len(crossings) == 1:
            beta2_out.append(axis[j])
            beta1_out.append(crossings[0])
        else:
            multi_crossing.append((j, len(crossings)))

    return LevelCurve(
        beta2=np.array(beta2_out),
        beta1=np.array(beta1_out),
        no_crossing_columns=no_crossing,
        multi_crossing_columns=multi_crossing,
    )


def d_field_full_model(solution: BeliefGrid2DWarmSolution, p: int, w: int) -> np.ndarray:
    """Q-difference field for the full unconstrained (4-action) model at a
    fixed context `(p, w)`: `min_{k: B-ish} Q(.,p,w,k) - min_{k: A-ish}
    Q(.,p,w,k)`. Unlike the always-warm model's `d`, this is
    context-specific (context-dependent switch costs are already baked into
    `Q`), so the decision boundary for this context is directly this
    field's zero level set -- no +-c_switch offset needed.

    No proof backs monotonicity of this field. The reason the always-warm
    proof doesn't transfer: that proof's induction closes because `base(beta,
    a)` uses the *same* update kernel (observation always happens) regardless
    of which action/context led here, so `Delta = h(.,B)-h(.,A)` reduces to a
    clean clamp of a single monotone `d`. Here, action-dependent observability
    means comparing Q across actions also compares continuation values under
    *different* observation regimes (e.g. `(A,cold)`'s deterministic predict
    vs. `(A,warm)`/`(B,.)`'s Bayes-updated expectation) -- the gap between
    them is a value-of-information term, bounded in sign by Jensen (h is
    separately concave) but hump-shaped in magnitude (near-zero at beta in
    {0,1}, largest at intermediate uncertainty). A hump-shaped term inside a
    Q-difference breaks the increasing-differences condition Topkis'
    theorem needs, so this field's monotonicity -- and hence a clean single-
    threshold-per-column curve -- is only ever checked numerically here
    (`check_monotone_grid`), never proven."""
    aish = [k for k, (a, _m) in enumerate(ACTIONS) if a == ACTION_A]
    bish = [k for k, (a, _m) in enumerate(ACTIONS) if a == ACTION_B]
    q_pw = solution.q[:, p, w, :]
    return q_pw[:, bish].min(axis=1) - q_pw[:, aish].min(axis=1)


__all__ = [
    "CONTEXT_A",
    "CONTEXT_B",
    "AlwaysWarmSolution",
    "always_warm_value_iteration",
    "AlwaysColdSolution",
    "always_cold_value_iteration",
    "verify_clamp_identity",
    "check_monotone_grid",
    "localize_monotonicity_violations",
    "LevelCurve",
    "extract_level_curve",
    "d_field_full_model",
]
