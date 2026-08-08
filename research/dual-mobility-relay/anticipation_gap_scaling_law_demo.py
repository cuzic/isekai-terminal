"""Tests the SCALING LAW question opus-symbolic-advisor identified as the real substance test
for the "anticipation gap" direction (2026-07-19): does the value of anticipation (captured here
via tau*(0), the true optimal FIRST wait before any observation, relative to the mean failure
time T_mean) scale cleanly with the prior's SHARPNESS -- i.e. with the coefficient of variation
CoV = sqrt((1-q)/K) of the phase-type (sum-of-K-geometrics) failure-time distribution?

Prediction to test: for K=1 (memoryless/exponential, CoV=1, no "schedule" information at all),
tau*(0) should be UNRELATED to T_mean specifically (governed only by the constant hazard rate).
For large K (CoV->0, a near-deterministic scheduled cutoff), tau*(0) should approach something
close to T_mean itself (the anticipation gap becomes maximally valuable -- you should wait almost
exactly until the expected cutoff, then start probing). If (T_mean - tau*(0)) / T_mean scales
cleanly as a function of CoV (e.g. proportionally), that IS the clean quantitative "value of
schedule-sharpness" law the advisor flagged as the actual substance test for this whole direction
(more meaningful than the shape of the transient trajectory, which is largely definitional/
already-known per the IFR-inspection literature).

Holds T_mean = K/q fixed at 50 across all K tested (so q = K/50), varying K in
{1, 2, 4, 8, 16, 32, 64}.

Run with: uv run python anticipation_gap_scaling_law_demo.py
"""

from __future__ import annotations

import numpy as np

C_PROBE = 1.0
C_DELAY_PER_STEP = 0.15
T_MEAN = 200.0
MAX_WAIT = 1000


def build_phase_dynamics(k_phases: int, q: float) -> np.ndarray:
    n = k_phases + 1
    T_mat = np.zeros((n, n))
    for i in range(k_phases):
        T_mat[i, i] = 1 - q
        T_mat[i, i + 1] = q
    T_mat[k_phases, k_phases] = 1.0
    return T_mat


def true_optimal_wait_from_start(k_phases: int, q: float, max_wait: int = MAX_WAIT) -> tuple[int, float]:
    """tau*(0): the true optimal FIRST wait, starting from phase 0 with certainty (t=0, no
    information yet) -- computed by direct forward simulation of the belief and cost."""
    T_mat = build_phase_dynamics(k_phases, q)
    belief = np.zeros(k_phases + 1)
    belief[0] = 1.0

    best_wait, best_rate = None, float("inf")
    b = belief.copy()
    cum_delay = 0.0
    for wait in range(1, max_wait):
        cum_delay += b[k_phases] * C_DELAY_PER_STEP
        rate = (cum_delay + C_PROBE) / wait
        if rate < best_rate:
            best_rate, best_wait = rate, wait
        b = b @ T_mat
    return best_wait, best_rate


def coefficient_of_variation(k_phases: int, q: float) -> float:
    """CoV of a sum of k_phases iid Geometric(q) random variables."""
    mean = k_phases / q
    var = k_phases * (1 - q) / q**2
    return float(np.sqrt(var) / mean)


def main() -> None:
    print(f"T_mean fixed at {T_MEAN} across all K (q = K/{T_MEAN})\n")
    print(f"{'K':>4} {'q':>8} {'CoV':>8} {'tau*(0)':>9} {'T_mean-tau*(0)':>15} "
          f"{'(T_mean-tau*)/T_mean':>21} {'ratio/CoV':>11} {'gap/std':>10}")

    results = []
    for k_phases in [1, 2, 4, 8, 16, 32, 64, 100, 140, 170, 190, 199]:
        q = k_phases / T_MEAN
        cov = coefficient_of_variation(k_phases, q)
        std = cov * T_MEAN
        tau_star, _ = true_optimal_wait_from_start(k_phases, q)
        gap_from_mean = T_MEAN - tau_star
        rel_gap = gap_from_mean / T_MEAN
        ratio_to_cov = rel_gap / cov if cov > 0 else float("nan")
        gap_over_std = gap_from_mean / std if std > 0 else float("nan")
        results.append((k_phases, q, cov, tau_star, gap_from_mean, rel_gap, ratio_to_cov, gap_over_std))
        print(f"{k_phases:>4} {q:>8.4f} {cov:>8.4f} {tau_star:>9} {gap_from_mean:>15.2f} "
              f"{rel_gap:>21.4f} {ratio_to_cov:>11.4f} {gap_over_std:>10.4f}")

    print("\nIf '(T_mean-tau*(0))/T_mean' scales linearly with CoV (i.e. 'ratio/CoV' column is")
    print("roughly CONSTANT across K), that is the clean scaling law: anticipation value is")
    print("proportional to prior sharpness (1/CoV), a genuine quantitative substance result.")


if __name__ == "__main__":
    main()
