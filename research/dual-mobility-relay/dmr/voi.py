"""Exact one-shot value-of-information (Blackwell) gap between two channels.

By Blackwell's theorem on comparison of experiments: if observation channel
`o_composite = f(o_decomp)` is a deterministic garbling of `o_decomp` (as it
is here -- composite is literally "hop1 loss OR hop2 loss"), then for *any*
decision problem (any state-and-action loss function Q), the Bayes risk of
acting on o_decomp is never higher than the Bayes risk of acting on
o_composite. This module computes that Bayes risk exactly in closed form
for a single decision -- no Monte Carlo, no belief-tracking dynamics, no
simulation noise -- given a prior P(state) (e.g. the channel's stationary
distribution) and a per-state/per-action Q-table.

This is a *different*, cleaner quantity than `switching.simulate_belief_policy_switch`'s
long-run Monte Carlo estimate: it measures the value of a single observation
drawn fresh from the prior each time, not the value of tracking a belief
across many consecutive correlated observations of the same underlying
trajectory. It isolates exactly "does this garbling cross a decision
boundary", with none of the sequential belief-tracking confounds (e.g. the
policy_eval "stuck on path A" absorbing-state effect).
"""

from __future__ import annotations

import numpy as np


def bayes_risk(prior: np.ndarray, obs_likelihood: np.ndarray, q_slice: np.ndarray) -> float:
    """Exact one-shot Bayes risk of the optimal action given an observation.

    prior: (n_c,) P(state).
    obs_likelihood: (n_c, n_obs) P(obs | state).
    q_slice: (n_c, n_actions) loss-to-go for a fixed context (e.g. Q[:, active, :]).
    Returns E_o[ min_a E_{state|o}[Q(state, a)] ].
    """
    joint = prior[:, None] * obs_likelihood  # (n_c, n_obs)
    p_o = joint.sum(axis=0)
    risk = 0.0
    for o in range(obs_likelihood.shape[1]):
        if p_o[o] <= 0:
            continue
        posterior = joint[:, o] / p_o[o]
        expected_q = posterior @ q_slice  # (n_actions,)
        risk += p_o[o] * expected_q.min()
    return float(risk)


def prior_risk(prior: np.ndarray, q_slice: np.ndarray) -> float:
    """Risk with no observation at all (act on the prior alone) -- an
    upper bound both bayes_risk(...) values must sit below."""
    expected_q = prior @ q_slice
    return float(expected_q.min())


def decomposition_value_gap(
    prior: np.ndarray,
    comp_lik: np.ndarray,
    decomp_lik: np.ndarray,
    q_slice: np.ndarray,
) -> float:
    """Risk(composite) - Risk(decomp) >= 0 always, by Blackwell's theorem
    (composite is a deterministic garbling of decomp)."""
    return bayes_risk(prior, comp_lik, q_slice) - bayes_risk(prior, decomp_lik, q_slice)


__all__ = ["bayes_risk", "prior_risk", "decomposition_value_gap"]
