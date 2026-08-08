"""Belief-simplex grid value iteration, replacing QMDP with a near-exact
POMDP solve for the warm-standby model.

QMDP is exact only when actions don't affect either the transition or the
observation process (see policy_eval.py's docstring). Since fixing the
observation model to be action-dependent (2026-07-17 formalization review:
warming the standby now genuinely buys information, not just a cheaper
switch), QMDP's continuation term implicitly assumes the state becomes
fully known one step later -- so it structurally cannot value "pay to
observe" at all. Because the hidden channel space here has only 4 states,
exact value iteration over a discretized belief simplex is entirely
tractable.

Grid: all points b in the probability simplex over `n_categories` outcomes
whose coordinates are multiples of 1/resolution, represented as integer
count-vectors summing to `resolution`. Interpolation at an arbitrary belief
point uses `scipy.spatial.Delaunay`'s barycentric coordinates over the
grid's first (n_categories - 1) coordinates (the last is determined by the
simplex constraint) -- a standard, exact (not hand-rolled) triangulation +
barycentric-interpolation scheme. An earlier hand-rolled "Freudenthal
triangulation" attempt had a real bug (its candidate vertices didn't all
sum to `resolution`, i.e. weren't valid lattice points) caught by a direct
reconstruction check before it was ever used downstream; Delaunay + qhull
avoids re-deriving that combinatorics by hand.
"""

from __future__ import annotations

import numpy as np
from scipy.spatial import Delaunay


def lattice_points(n_categories: int, resolution: int) -> np.ndarray:
    """All integer count-vectors of length `n_categories` summing to
    `resolution` -- the belief-simplex grid points (belief = counts /
    resolution). Shape (n_points, n_categories)."""
    points: list[list[int]] = []

    def rec(remaining: int, k: int, acc: list[int]) -> None:
        if k == 1:
            points.append(acc + [remaining])
            return
        for i in range(remaining + 1):
            rec(remaining - i, k - 1, acc + [i])

    rec(resolution, n_categories, [])
    return np.array(points, dtype=int)


class BeliefGrid:
    """Lattice points over the probability simplex plus a Delaunay
    triangulation (over the first n_categories-1 coordinates) for exact
    barycentric interpolation of any function known at the lattice points."""

    def __init__(self, n_categories: int, resolution: int):
        self.n_categories = n_categories
        self.resolution = resolution
        self.points = lattice_points(n_categories, resolution)  # (n_points, n_categories) ints
        self.beliefs = self.points / resolution  # (n_points, n_categories) floats, rows sum to 1
        # Triangulate in the reduced (n_categories - 1)-dim coordinate space;
        # the last coordinate is implied by the simplex constraint.
        self._tri = Delaunay(self.beliefs[:, :-1])

    def interpolation_weights(self, belief: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        """Return (vertex_indices, weights) into `self.points`/`self.beliefs`
        for an arbitrary belief point: non-negative weights summing to 1
        such that sum(w_i * f(vertex_i)) is the exact barycentric
        interpolation of any affine f, for any point inside the grid's
        convex hull (the full simplex, so always true here up to float
        round-off at the boundary)."""
        query = np.clip(np.asarray(belief, dtype=float)[:-1], 0.0, 1.0)
        simplex_idx = self._tri.find_simplex(query)
        if simplex_idx < 0:
            # Numerical edge case (point just outside the hull due to
            # floating point noise at a simplex boundary/vertex) -- nudge
            # slightly inward and retry once.
            centroid = self.beliefs[:, :-1].mean(axis=0)
            nudged = query + 1e-9 * (centroid - query)
            simplex_idx = self._tri.find_simplex(nudged)
            query = nudged
        vertex_ids = self._tri.simplices[simplex_idx]
        transform = self._tri.transform[simplex_idx]
        delta = query - transform[-1]
        bary_partial = transform[:-1] @ delta
        weights = np.append(bary_partial, 1.0 - bary_partial.sum())
        weights = np.clip(weights, 0.0, None)
        weights = weights / weights.sum()
        return vertex_ids, weights

    def interpolate(self, values: np.ndarray, belief: np.ndarray) -> float:
        """Interpolate a function known at all lattice points (`values`,
        indexed the same way as `self.points`/`self.beliefs`) at an
        arbitrary belief point."""
        vertex_ids, weights = self.interpolation_weights(belief)
        return float(weights @ values[vertex_ids])

    def interpolate_batch(self, values: np.ndarray, belief_batch: np.ndarray) -> np.ndarray:
        """Vectorized `interpolate` over many query points at once (shape
        (n_queries, n_categories) -> (n_queries,)). This is the version
        actually used by value iteration -- batching is what makes a belief
        grid of a few hundred/thousand points tractable, since a Python-level
        loop calling `interpolate` once per (grid point, context, action,
        observation) combination would be orders of magnitude slower."""
        query = np.clip(belief_batch[:, :-1], 0.0, 1.0)
        simplex_idx = self._tri.find_simplex(query)

        bad = simplex_idx < 0
        if np.any(bad):
            centroid = self.beliefs[:, :-1].mean(axis=0)
            nudged = query[bad] + 1e-9 * (centroid - query[bad])
            simplex_idx[bad] = self._tri.find_simplex(nudged)
            query[bad] = nudged

        vertex_ids = self._tri.simplices[simplex_idx]  # (n_queries, n_categories)
        transform = self._tri.transform[simplex_idx]  # (n_queries, n_categories, n_categories-1)
        delta = query - transform[:, -1, :]  # (n_queries, n_categories-1)
        bary_partial = np.einsum("qij,qj->qi", transform[:, :-1, :], delta)
        weights = np.concatenate(
            [bary_partial, 1.0 - bary_partial.sum(axis=1, keepdims=True)], axis=1
        )
        weights = np.clip(weights, 0.0, None)
        weights = weights / weights.sum(axis=1, keepdims=True)

        return np.einsum("qv,qv->q", weights, values[vertex_ids])


__all__ = ["lattice_points", "BeliefGrid"]
