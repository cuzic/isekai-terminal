"""Minimal exact belief-grid solver for the M=2-candidate-relay-vehicle case
(task #59, prerequisite for #51): 3 routing alternatives -- the direct path
`A`, and two candidate relay vehicles `R1`/`R2` -- rather than `dmr/nhop.py`'s
single relay path with n *serial* hops and a *binary* routing choice.
`nhop.py` cannot be reused here: `min` over 2 branches collapses to a single
scalar clamp field (the always-warm theorem's mechanism), but `min` over 3+
branches does not reduce the same way (see `nhop.py`'s module docstring;
Jun 2004, Glazebrook/Ruiz-Hernandez/Kirkbride 2006 on why 3+-armed switching
is restless-bandit territory in general).

Scope, per Fable's rescoping advice (2026-07-18) for a first, tractable cut:
- Each candidate relay is modeled as a single Gilbert-Elliott channel (not a
  multi-hop composite) -- belief is `(beta1, beta2) = P(R1=Bad), P(R2=Bad)`,
  the same 2D grid as `beliefgrid2d.py`, just relabeled: here beta_k is
  belief about relay k's OWN channel state, not about a shared path's k-th
  hop.
- Always-warm-on-all-arms: both relay channels are observed every step
  regardless of which route is actually carrying traffic (mirrors
  `switching_curves.py`'s always-warm sub-model's simplification -- no
  action-dependent observability at all, hence no Gap-G1-style VoI-hump
  obstruction here by construction. This is a deliberate first cut, not a
  claim that always-warm-on-all-arms is realistic).
- Uniform switching cost `c_switch` between any pair of distinct routes (A,
  R1, R2), regardless of which two -- not yet distinguishing "A-to-R1" from
  "R1-to-R2" costs.

Context = currently active route (0=A, 1=R1, 2=R2); the "stay-region" for
context c is `{belief : optimal next route == c}`.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import channels
from .beliefgrid2d import RegularGrid2D, bayes_update_scalar, obs_prob_scalar, predict_scalar

ROUTE_A, ROUTE_R1, ROUTE_R2 = 0, 1, 2
N_ROUTES = 3


@dataclass(frozen=True)
class MHopRelaySolution:
    grid: RegularGrid2D
    relay1: channels.HopParams
    relay2: channels.HopParams
    cost_a: float
    c_switch: float
    h: np.ndarray  # (n_points, 3): relative value, h[:, context] for context in {A=0, R1=1, R2=2}
    g: float
    q: np.ndarray  # (n_points, 3, 3): relative Q(belief, current_route, next_route)
    policy: np.ndarray  # (n_points, 3): argmin next_route in each current-route context


def _continuation(
    grid: RegularGrid2D, relay1: channels.HopParams, relay2: channels.HopParams, h_slice: np.ndarray
) -> np.ndarray:
    """E[h_slice(next belief)] under always-warm-on-all-arms: both relay
    channels are observed every step regardless of route, so there is no
    "if observable" branch at all (identical in structure to
    `switching_curves._continuation_always_warm`, just over two independent
    relay channels instead of two serial hops of one path)."""
    b1, b2 = grid.beta1, grid.beta2
    cont = np.zeros(grid.n_points)
    for l1 in (0, 1):
        p1 = obs_prob_scalar(b1, relay1, l1)
        b1_next = predict_scalar(bayes_update_scalar(b1, relay1, l1), relay1)
        for l2 in (0, 1):
            p2 = obs_prob_scalar(b2, relay2, l2)
            b2_next = predict_scalar(bayes_update_scalar(b2, relay2, l2), relay2)
            interp_vals = grid.interpolate_batch(h_slice, b1_next, b2_next)
            cont += p1 * p2 * interp_vals
    return cont


def mhop_relay_value_iteration(
    relay1: channels.HopParams,
    relay2: channels.HopParams,
    cost_a: float,
    c_switch: float,
    resolution: int = 100,
    ref_point: tuple[float, float] = (0.5, 0.5),
    ref_context: int = ROUTE_A,
    n_iters: int = 2000,
    tol: float = 1e-9,
) -> MHopRelaySolution:
    """RVI value iteration for the M=2-relay-arm sub-model: 3 routing
    alternatives, always-warm-on-all-arms, uniform switching cost."""
    grid = RegularGrid2D(resolution)
    content_a = np.full(grid.n_points, cost_a)
    content_r1 = obs_prob_scalar(grid.beta1, relay1, loss=1)
    content_r2 = obs_prob_scalar(grid.beta2, relay2, loss=1)
    content = [content_a, content_r1, content_r2]

    ref_b1, ref_b2 = ref_point
    ref_index = int(np.argmin((grid.beta1 - ref_b1) ** 2 + (grid.beta2 - ref_b2) ** 2))

    h = np.zeros((grid.n_points, N_ROUTES))
    g = 0.0
    for _ in range(n_iters):
        cont = [_continuation(grid, relay1, relay2, h[:, r]) for r in range(N_ROUTES)]
        base = [content[r] + cont[r] for r in range(N_ROUTES)]
        q_full = np.stack(
            [np.stack([base[r] + (0.0 if r == p else c_switch) for r in range(N_ROUTES)], axis=1)
             for p in range(N_ROUTES)],
            axis=1,
        )  # (n_points, current_route p, next_route r)
        h_full = q_full.min(axis=2)

        g_new = float(h_full[ref_index, ref_context])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            break

    cont = [_continuation(grid, relay1, relay2, h[:, r]) for r in range(N_ROUTES)]
    base = [content[r] + cont[r] for r in range(N_ROUTES)]
    q_full = np.stack(
        [np.stack([base[r] + (0.0 if r == p else c_switch) for r in range(N_ROUTES)], axis=1)
         for p in range(N_ROUTES)],
        axis=1,
    )
    policy = q_full.argmin(axis=2)

    return MHopRelaySolution(
        grid=grid, relay1=relay1, relay2=relay2, cost_a=cost_a, c_switch=c_switch,
        h=h, g=g, q=q_full, policy=policy,
    )


def stay_region_monotone_check(solution: MHopRelaySolution, context: int, tol: float = 1e-9) -> dict:
    """Detects FLICKER (3+ transitions -- entering and exiting the stay-
    region more than once) in `{belief : policy[belief, context] ==
    context}` per 1D slice. NOTE (task #68): despite the "monotone" name,
    this is NOT a strict "is the stay-region a single connected interval"
    check -- a transition count of exactly 2 is consistent with BOTH a
    single clean interval (the benign, conjecture-satisfying case) AND a
    `[1,0,1]`-style 2-component disconnection (stay at both edges of the
    slice, gap in the middle), since both produce exactly 2 sign changes.
    Use `stay_region_connected_components_check` for the strict single-
    interval property; use this function only for detecting 3+-transition
    flicker specifically. Returns violation counts per axis, analogous to
    `switching_curves.check_monotone_grid` but on a boolean membership field
    via transition counting rather than a real-valued monotonicity check.

    BUG FIXED 2026-07-18 (caught by an independent Fable-model review): this
    used to return `max(0, transitions - 1)`, which flags a completely
    benign single interior interval (2 transitions: one enter, one exit) as
    1 "violation" -- only 3+ transitions is genuine flickering. The original
    150-scenario search's headline "80/150 (53%) scenarios violate" figure
    was an artifact of this off-by-one: re-counted with the fix, genuine
    flickering (3+ transitions) occurs in only 9/150 (6%) scenarios --
    the same order as the binary-routing model's Gap G1 counterexample rate
    (12/250, 4.8%), not dramatically higher. See MHOP_RELAY_NOTES.md's
    correction note for the full account; `mhop_relay_search_demo.py`'s
    resolution-convergence numbers for the ORIGINAL worst case (23/48/82)
    were also counting benign intervals, not genuine flicker."""
    stay = (solution.policy[:, context] == context).astype(int).reshape(solution.grid.shape)

    def _count_multi_transitions(arr_along_axis: np.ndarray) -> int:
        diffs = np.diff(arr_along_axis)
        transitions = np.count_nonzero(diffs)
        return max(0, transitions - 2)

    violations_beta1 = sum(_count_multi_transitions(stay[:, j]) for j in range(stay.shape[1]))
    violations_beta2 = sum(_count_multi_transitions(stay[i, :]) for i in range(stay.shape[0]))
    return {
        "n_multi_transition_columns_beta1": violations_beta1,
        "n_multi_transition_rows_beta2": violations_beta2,
    }


def _count_true_runs(arr_along_axis: np.ndarray) -> int:
    """Number of maximal contiguous runs of `True`(1) in a 0/1 array. A
    single interval (whether touching an edge or fully interior) has
    exactly 1 run; a fully-`False` slice has 0 (vacuously fine, no interval
    to speak of); 2+ runs means the stay-region is DISCONNECTED on that
    slice, e.g. `[1,0,1]` (stay at both edges, gap in the middle) -- which
    `stay_region_monotone_check`'s transition count alone cannot distinguish
    from a single clean interval, since both have exactly 2 transitions."""
    padded = np.concatenate(([0], arr_along_axis.astype(int), [0]))
    diff = np.diff(padded)
    return int(np.count_nonzero(diff == 1))


def stay_region_connected_components_check(solution: MHopRelaySolution, context: int) -> dict:
    """Strict "is the stay-region a single connected interval per 1D slice"
    check (task #68, added per Codex's 2026-07-18 review): unlike
    `stay_region_monotone_check` (which only flags 3+ transitions as
    "flicker" and cannot tell a single clean interval apart from a
    2-component split like `[1,0,1]`, both of which have exactly 2
    transitions), this counts contiguous `True`-runs directly via
    `_count_true_runs` -- any slice with more than 1 run is a genuine
    disconnection of the stay-region, regardless of its transition count.
    Returns violation counts (columns/rows with >1 run) per axis, plus the
    max run-count seen (useful to distinguish "barely disconnected" from
    "wildly fragmented")."""
    stay = (solution.policy[:, context] == context).astype(int).reshape(solution.grid.shape)

    beta1_run_counts = [_count_true_runs(stay[:, j]) for j in range(stay.shape[1])]
    beta2_run_counts = [_count_true_runs(stay[i, :]) for i in range(stay.shape[0])]
    return {
        "n_disconnected_columns_beta1": sum(1 for c in beta1_run_counts if c > 1),
        "n_disconnected_rows_beta2": sum(1 for c in beta2_run_counts if c > 1),
        "max_run_count_beta1": max(beta1_run_counts, default=0),
        "max_run_count_beta2": max(beta2_run_counts, default=0),
    }


__all__ = [
    "ROUTE_A",
    "ROUTE_R1",
    "ROUTE_R2",
    "N_ROUTES",
    "MHopRelaySolution",
    "mhop_relay_value_iteration",
    "stay_region_monotone_check",
    "stay_region_connected_components_check",
]
