"""Exact HMM belief filtering over the 4-state joint channel.

Standard forward-algorithm recursion:

    b_predict = b_prev @ T                      # predict through transition
    b_new_i  ~= b_predict_i * P(o | state=i)    # Bayes update on observation
    b_new     = b_new / sum(b_new)              # normalize

Because the state space has only 4 states, this is exact (no particle
filter / approximation needed).
"""

from __future__ import annotations

import numpy as np


def initial_belief() -> np.ndarray:
    return np.full(4, 0.25)


def predict(belief: np.ndarray, t: np.ndarray) -> np.ndarray:
    return belief @ t


def update(belief_pred: np.ndarray, obs_likelihood_row: np.ndarray) -> np.ndarray:
    unnorm = belief_pred * obs_likelihood_row
    total = unnorm.sum()
    if total <= 0.0:
        # Numerically degenerate (shouldn't happen with valid likelihoods);
        # fall back to the prediction to avoid propagating NaNs.
        return belief_pred
    return unnorm / total


def step(belief: np.ndarray, t: np.ndarray, obs_likelihood: np.ndarray, obs: int) -> np.ndarray:
    """One predict+update step given a realized observation index `obs`."""
    belief_pred = predict(belief, t)
    return update(belief_pred, obs_likelihood[:, obs])


def filter_sequence(
    t: np.ndarray, obs_likelihood: np.ndarray, obs_sequence: np.ndarray
) -> np.ndarray:
    """Run the filter over a full observation sequence.

    Returns beliefs of shape (len(obs_sequence), 4), beliefs[k] is the
    posterior after observing obs_sequence[k] (predict-then-update at each
    step, starting from a uniform prior).
    """
    belief = initial_belief()
    beliefs = np.zeros((len(obs_sequence), 4))
    for k, o in enumerate(obs_sequence):
        belief = step(belief, t, obs_likelihood, int(o))
        beliefs[k] = belief
    return beliefs
