"""Decisive novelty test for the "anticipatory probing" direction (2026-07-19, per
opus-symbolic-advisor): rather than claim the transient (stage-2 shrinking interval) itself is
novel -- likely already qualitatively known in IFR (increasing-failure-rate) optimal inspection
theory -- pin the novelty down to a specific, quantitative, well-defined gap:

  anticipation_gap(t) := tau*(t) - f(h(t))

where `tau*(t)` is the TRUE optimal renewal wait computed from the full belief at time t
(correctly accounting for how the hazard will EVOLVE over the wait), and `f(h(t))` is the naive
"myopic" optimal wait a decision-maker would pick if they treated the CURRENT instantaneous hazard
h(t) as constant forever (i.e. solved the standard memoryless renewal problem with that single
number, ignoring that the hazard is about to change).

Prediction (advisor's): this gap should be ~0 at the quasi-stationary plateau (constant hazard,
so myopic = optimal there) and become meaningfully NEGATIVE (true optimal probes SOONER than the
myopic rule would) during the hazard-rising transient -- since a rising hazard means waiting is
riskier than the current snapshot alone suggests. A gap that's essentially always ~0 (myopic ==
optimal throughout) would mean the earlier "3-stage" structure was just quasi-static tracking of
the current hazard, not genuine non-myopic anticipation -- a much weaker (and likely
already-known, cf. IFR inspection theory) finding.

Run with: uv run python anticipation_gap_demo.py
"""

from __future__ import annotations

import numpy as np

K_PHASES = 6
Q = 0.12
C_PROBE = 1.0
C_DELAY_PER_STEP = 0.15


def build_phase_dynamics():
    n = K_PHASES + 1
    T_mat = np.zeros((n, n))
    for i in range(K_PHASES):
        T_mat[i, i] = 1 - Q
        T_mat[i, i + 1] = Q
    T_mat[K_PHASES, K_PHASES] = 1.0
    return T_mat


T_MAT = build_phase_dynamics()


def instantaneous_hazard(belief_vec: np.ndarray) -> float:
    """P(fail at the very next step | current belief over phases, conditioned on not-yet-failed).
    Only phase K_PHASES-1 has a nonzero transition probability to Failed."""
    return float(belief_vec[K_PHASES - 1] * Q)


def expected_delay_cost_if_wait(belief_vec: np.ndarray, wait: int) -> float:
    b = belief_vec.copy()
    total = 0.0
    for _ in range(wait):
        total += b[K_PHASES] * C_DELAY_PER_STEP
        b = b @ T_MAT
    return total


def true_optimal_wait(belief_vec: np.ndarray, max_wait: int = 300) -> tuple[int, float]:
    """tau*(t): the TRUE optimal wait, using the FULL belief propagation (accounts for how the
    hazard will actually evolve over the wait -- non-myopic by construction)."""
    best_wait, best_rate = None, float("inf")
    for wait in range(1, max_wait):
        cost = expected_delay_cost_if_wait(belief_vec, wait) + C_PROBE
        rate = cost / wait
        if rate < best_rate:
            best_rate, best_wait = rate, wait
    return best_wait, best_rate


def myopic_optimal_wait_for_constant_hazard(h: float, max_wait: int = 300) -> int:
    """f(h(t)): the optimal wait if a decision-maker (wrongly) assumed the CURRENT instantaneous
    hazard h stays constant (memoryless) forever -- i.e. ignores the true, evolving belief and
    just plugs h into the standard constant-hazard renewal formula."""
    if h <= 1e-12:
        return max_wait  # zero current hazard -> myopic says "no rush at all", saturates at cap
    best_wait, best_rate = None, float("inf")
    surv = 1.0
    cum_delay = 0.0
    for wait in range(1, max_wait):
        p_failed_by_prev = 1 - surv
        cum_delay += p_failed_by_prev * C_DELAY_PER_STEP
        surv *= (1 - h)
        rate = (cum_delay + C_PROBE) / wait
        if rate < best_rate:
            best_rate, best_wait = rate, wait
    return best_wait


def conditional_belief_given_not_failed(belief_vec: np.ndarray) -> np.ndarray:
    non_failed = belief_vec[:K_PHASES].copy()
    total = non_failed.sum()
    if total < 1e-15:
        return non_failed
    return non_failed / total


def main() -> None:
    print(f"K={K_PHASES}, q={Q}, C_PROBE={C_PROBE}, C_DELAY={C_DELAY_PER_STEP}\n")
    print(f"{'cycle':>6} {'phase_mean':>11} {'h(t)_instant':>13} {'tau*(true)':>11} "
          f"{'f(h(t))_myopic':>15} {'gap=tau*-f(h)':>14}")

    belief = np.zeros(K_PHASES + 1)
    belief[0] = 1.0

    for cycle in range(25):
        phase_mean = float(np.dot(np.arange(K_PHASES + 1), belief))
        h_now = instantaneous_hazard(belief)
        tau_star, _ = true_optimal_wait(belief)
        f_h = myopic_optimal_wait_for_constant_hazard(h_now)
        gap = tau_star - f_h
        print(f"{cycle:>6} {phase_mean:>11.4f} {h_now:>13.6f} {tau_star:>11} {f_h:>15} {gap:>+14}")

        propagated = belief @ np.linalg.matrix_power(T_MAT, tau_star)
        b2 = conditional_belief_given_not_failed(propagated)
        full = np.zeros(K_PHASES + 1)
        full[:K_PHASES] = b2
        belief = full
        if belief[:K_PHASES].sum() < 1e-12:
            break


if __name__ == "__main__":
    main()
