"""Mutual information / KL divergence between hidden state and observation.

`o_composite = l1 OR l2` is a deterministic coarsening of
`o_decomp = (l1, l2)`, so by the data processing inequality
I(X; O_decomp) >= I(X; O_composite) always. This module computes both sides
exactly (closed-form on the small discrete joint distribution) so the gap
can be characterized as a function of the channel parameters.
"""

from __future__ import annotations

import numpy as np


def _entropy(p: np.ndarray) -> float:
    p = p[p > 0]
    return float(-np.sum(p * np.log2(p)))


def mutual_information(prior: np.ndarray, obs_likelihood: np.ndarray) -> float:
    """I(X; O) given P(X=x)=prior[x] and P(O=o|X=x)=obs_likelihood[x, o].

    Computed as H(O) - H(O|X), both from the same joint distribution, so no
    approximation is involved beyond floating point.
    """
    joint = prior[:, None] * obs_likelihood  # joint[x, o] = P(x, o)
    p_o = joint.sum(axis=0)
    h_o = _entropy(p_o)
    h_o_given_x = float(-np.sum(joint[joint > 0] * np.log2(obs_likelihood[joint > 0])))
    return h_o - h_o_given_x


def mutual_information_via_expected_kl(prior: np.ndarray, obs_likelihood: np.ndarray) -> float:
    """Same quantity via I(X;O) = E_o[KL(P(X|o) || P(X))], as a cross-check."""
    joint = prior[:, None] * obs_likelihood
    p_o = joint.sum(axis=0)
    mi = 0.0
    for o in range(obs_likelihood.shape[1]):
        if p_o[o] <= 0:
            continue
        posterior = joint[:, o] / p_o[o]
        mi += p_o[o] * kl_divergence(posterior, prior)
    return float(mi)


def kl_divergence(p: np.ndarray, q: np.ndarray) -> float:
    mask = p > 0
    return float(np.sum(p[mask] * np.log2(p[mask] / q[mask])))
