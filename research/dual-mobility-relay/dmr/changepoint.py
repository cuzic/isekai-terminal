"""Sequential change-point detection for "which hop degraded".

Two mechanisms are implemented, both operating on hop-decomposed
observations (they are not possible on the composite observation alone,
since a composite loss event doesn't reveal which hop caused it):

- `cusum_hop_detector`: a classic Page-CUSUM log-likelihood-ratio detector
  run independently per hop on that hop's own loss bit stream. Cheap,
  online, O(1) per step.
- `belief_hop_attribution`: reuses the exact 4-state Bayesian filter
  (`filtering.py`) and reads off the marginal posterior P(hop_i = Bad) at
  each step. This is the "exact filter" alternative to CUSUM/BOCPD promised
  for a state space this small -- no particle filter or approximation
  needed, it's just a projection of the already-exact joint belief.
"""

from __future__ import annotations

import numpy as np


def cusum_hop_detector(
    loss_bits: np.ndarray, eps_good: float, eps_bad: float, threshold: float
) -> tuple[np.ndarray, np.ndarray]:
    """Page-CUSUM on a single hop's Bernoulli loss stream.

    Returns (statistic, alarms) where `statistic[t]` is the running
    log-likelihood-ratio statistic and `alarms[t]` is True at steps where it
    crosses `threshold` (the statistic resets to 0 right after an alarm).
    """
    llr_loss = np.log(eps_bad / eps_good)
    llr_no_loss = np.log((1 - eps_bad) / (1 - eps_good))
    increments = np.where(loss_bits, llr_loss, llr_no_loss)

    stat = np.zeros(len(loss_bits))
    alarms = np.zeros(len(loss_bits), dtype=bool)
    s = 0.0
    for k, inc in enumerate(increments):
        s = max(0.0, s + inc)
        if s > threshold:
            alarms[k] = True
            s = 0.0
        stat[k] = s
    return stat, alarms


def belief_hop_attribution(beliefs: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Marginal P(hop1=Bad), P(hop2=Bad) from joint beliefs of shape (T, 4).

    State index convention is s1*2 + s2 (see channels.py): states {1, 3}
    have hop2=Bad, states {2, 3} have hop1=Bad.
    """
    p_hop1_bad = beliefs[:, 2] + beliefs[:, 3]
    p_hop2_bad = beliefs[:, 1] + beliefs[:, 3]
    return p_hop1_bad, p_hop2_bad
