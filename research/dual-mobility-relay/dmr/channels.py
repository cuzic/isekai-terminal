"""Gilbert-Elliott hop channels and the joint 2-hop Markov chain.

Each hop is a 2-state (Good=0, Bad=1) Markov chain. The joint state of the
two hops is a 4-state chain over {GG, GB, BG, BB} (index = s1*2 + s2).

Inter-hop correlation is modeled with a single knob `rho` in [0, 1] that
mixes, for each current joint state, the *independent* coupling of the two
hops' marginal transition rows with the *comonotone* (Fréchet upper-bound)
coupling of those same rows -- the maximal-correlation coupling under the
Good(0) < Bad(1) order. Both couplings individually reproduce each hop's
exact marginal transition row (see `_comonotone_coupling`'s docstring for
the proof), so the mixture

    T(rho)[(s1,s2), :] = (1-rho) * independent(row1, row2)
                        + rho   * comonotone(row1, row2)

preserves each hop's own marginal dynamics -- burst length, stationary bad
probability -- exactly, for every rho. Only the co-occurrence of Bad states
changes with rho. This matters: an earlier version of this module instead
mixed towards "hop2's next state is forced to equal hop1's", which silently
overwrote hop2's own p_gb/p_bg at rho=1 and confounded the correlation sweep
with a change in hop2's burst length. This version was flagged in an
external formalization review (Codex + Fable, 2026-07-17) and replaced.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

GOOD, BAD = 0, 1
STATE_LABELS = ["GG", "GB", "BG", "BB"]


@dataclass(frozen=True)
class HopParams:
    """Gilbert-Elliott parameters for a single hop.

    p_gb: P(Good -> Bad) per step.
    p_bg: P(Bad -> Good) per step.
    eps_good: packet loss probability while in Good.
    eps_bad: packet loss probability while in Bad.
    """

    p_gb: float
    p_bg: float
    eps_good: float
    eps_bad: float

    def transition_matrix(self) -> np.ndarray:
        return np.array(
            [
                [1.0 - self.p_gb, self.p_gb],
                [self.p_bg, 1.0 - self.p_bg],
            ]
        )

    def stationary_bad_prob(self) -> float:
        return self.p_gb / (self.p_gb + self.p_bg)

    def mean_bad_burst_length(self) -> float:
        """Expected number of consecutive steps spent in Bad (geometric)."""
        return 1.0 / self.p_bg

    def loss_prob(self, state: np.ndarray) -> np.ndarray:
        return np.where(state == BAD, self.eps_bad, self.eps_good)


def _comonotone_coupling(row1: np.ndarray, row2: np.ndarray) -> np.ndarray:
    """Fréchet upper-bound (comonotone / maximal-correlation) coupling of two
    2-element distributions over {Good, Bad}, as a (2, 2) joint matrix
    indexed [next1, next2].

    For P(next1=Bad)=b1, P(next2=Bad)=b2:
        P(Bad,Bad) = min(b1, b2)
        P(Good,Good) = min(a1, a2) = 1 - max(b1, b2)
        P(Bad,Good) = max(b1 - b2, 0)
        P(Good,Bad) = max(b2 - b1, 0)

    This exactly marginalizes back to row1 and row2 for any b1, b2 in [0,1]
    (check the Bad-marginal: min(b1,b2) + max(b1-b2,0) = b1 whether b1 >= b2
    or not), and it maximizes P(next1=next2) among all couplings with these
    marginals (the standard Fréchet-Hoeffding upper bound).
    """
    a1, b1 = row1
    a2, b2 = row2
    joint = np.zeros((2, 2))
    joint[GOOD, GOOD] = min(a1, a2)
    joint[BAD, BAD] = min(b1, b2)
    joint[BAD, GOOD] = max(b1 - b2, 0.0)
    joint[GOOD, BAD] = max(b2 - b1, 0.0)
    return joint


def joint_transition_matrix(hop1: HopParams, hop2: HopParams, rho: float) -> np.ndarray:
    """Build the 4x4 joint transition matrix T(rho) over {GG, GB, BG, BB}.

    Each hop's own marginal transition row (hence its burst length and
    stationary bad probability) is preserved exactly for every rho -- only
    the correlation between the two hops' next states changes. See the
    module docstring and `_comonotone_coupling` for the construction.
    """
    t1 = hop1.transition_matrix()
    t2 = hop2.transition_matrix()

    t = np.zeros((4, 4))
    for s1 in range(2):
        for s2 in range(2):
            src = s1 * 2 + s2
            independent = np.outer(t1[s1], t2[s2])  # [next1, next2]
            comonotone = _comonotone_coupling(t1[s1], t2[s2])
            mix = (1.0 - rho) * independent + rho * comonotone
            for s1_next in range(2):
                for s2_next in range(2):
                    t[src, s1_next * 2 + s2_next] = mix[s1_next, s2_next]

    assert np.allclose(t.sum(axis=1), 1.0), "transition matrix rows must sum to 1"
    return t


def stationary_distribution(t: np.ndarray) -> np.ndarray:
    """Stationary distribution of a stochastic matrix via eigenvector."""
    eigvals, eigvecs = np.linalg.eig(t.T)
    idx = np.argmin(np.abs(eigvals - 1.0))
    vec = np.real(eigvecs[:, idx])
    if vec.sum() < 0:
        # eig() only fixes the eigenvector up to sign; the stationary
        # distribution's entries must all be non-negative, so flip if we
        # got the "all negative" representative.
        vec = -vec
    vec = np.clip(vec, 0.0, None)
    return vec / vec.sum()


def state_hop_indices(states: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Decompose joint state indices (0..3) into (hop1, hop2) state arrays."""
    states = np.asarray(states)
    return states // 2, states % 2


def path_b_loss_prob(hop1: HopParams, hop2: HopParams) -> np.ndarray:
    """P(packet lost on path B | joint state) for each of the 4 joint states."""
    s1, s2 = state_hop_indices(np.arange(4))
    e1 = hop1.loss_prob(s1)
    e2 = hop2.loss_prob(s2)
    return e1 + e2 - e1 * e2


def composite_obs_likelihood(hop1: HopParams, hop2: HopParams) -> np.ndarray:
    """P(o_composite=lost | state) as a (4, 2) matrix indexed [state, o]."""
    p_loss = path_b_loss_prob(hop1, hop2)
    lik = np.zeros((4, 2))
    lik[:, 1] = p_loss
    lik[:, 0] = 1.0 - p_loss
    return lik


def decomposed_obs_likelihood(hop1: HopParams, hop2: HopParams) -> np.ndarray:
    """P(o_decomp=(l1,l2) | state) as a (4, 4) matrix indexed [state, o].

    Observation index o = l1 * 2 + l2, matching the state index convention.
    Losses at hop1 and hop2 are conditionally independent given the state.
    """
    s1, s2 = state_hop_indices(np.arange(4))
    e1 = hop1.loss_prob(s1)
    e2 = hop2.loss_prob(s2)
    lik = np.zeros((4, 4))
    for l1 in range(2):
        for l2 in range(2):
            o = l1 * 2 + l2
            p1 = e1 if l1 else 1.0 - e1
            p2 = e2 if l2 else 1.0 - e2
            lik[:, o] = p1 * p2
    return lik
