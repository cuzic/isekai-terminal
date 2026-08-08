"""Minimal test case for the "anticipatory probing" question raised in the quickest-change-
detection direction (2026-07-19, per opus-symbolic-advisor's recommendation): before building a
full framework, check whether a NON-TRIVIAL optimal probing structure exists at all, or whether
the answer degenerates to "just probe once near the expected failure time" (which would mean no
real novelty over naive heuristics).

Setup: a failure time T follows a KNOWN, non-memoryless distribution (a discretized phase-type /
Erlang-like shape built from k sequential geometric phases, giving a "peaked around a mean, not
exponential" hazard profile -- e.g. representing "a guest WiFi network's ~60 minute scheduled
cutoff, with some jitter"). There is NO free signal (isolating the "informative non-exponential
prior + costly probing" structure specifically, per advisor's framing, from the separate free/
costly-channel duality). A perfect but costly probe (cost c_probe) reveals the true state when
used; if you don't probe, you accrue a delay cost c_delay per step you remain undetected after the
true failure has already occurred.

Solved via exact backward-induction dynamic programming over the belief (parameterized simply by
"time since last confirmed-good probe," since between probes there is no information at all, so
the belief evolves deterministically from the known phase-type transition structure -- directly
analogous to the cold-side park-length renewal structure used throughout this research thread,
but now with an AGE-DEPENDENT (non-stationary) hazard instead of a stationary Gilbert-Elliott one).

Run with: uv run python anticipatory_probing_minimal_case_demo.py
"""

from __future__ import annotations

import numpy as np

# Phase-type failure time: k sequential phases, each geometric(q) -- i.e. T = sum of k iid
# Geometric(q) increments, an Erlang-like (peaked, non-memoryless) discrete distribution.
K_PHASES = 6
Q = 0.12  # per-step phase-advance probability -> mean T = K/Q ~ 50 steps, meaningful spread
C_PROBE = 1.0
C_DELAY_PER_STEP = 0.15  # cost per step of remaining undetected after the true failure

HORIZON = 400  # steps; by this point survival probability is negligible


def build_phase_dynamics():
    """Returns the (K_PHASES+1)-state transition matrix (last state = Failed/absorbing)."""
    n = K_PHASES + 1
    T_mat = np.zeros((n, n))
    for i in range(K_PHASES):
        T_mat[i, i] = 1 - Q
        T_mat[i, i + 1] = Q
    T_mat[K_PHASES, K_PHASES] = 1.0  # absorbing
    return T_mat


T_MAT = build_phase_dynamics()


def survival_from_phase(belief_vec: np.ndarray, steps: int) -> np.ndarray:
    """Propagate a belief distribution over phases forward `steps` steps with no observation."""
    b = belief_vec.copy()
    M = np.linalg.matrix_power(T_MAT, steps)
    return b @ M


def expected_delay_cost_if_wait(belief_vec: np.ndarray, wait: int) -> float:
    """Expected cost accrued from delay, if we wait `wait` steps before the next probe, starting
    from belief_vec (a distribution over phases at "now"), integrating the delay cost over all
    steps we remain in the Failed state before the probe at t=wait catches it."""
    b = belief_vec.copy()
    total = 0.0
    for step in range(wait):
        # probability of being in Failed state at this exact step (before probing)
        p_failed_now = b[K_PHASES]
        total += p_failed_now * C_DELAY_PER_STEP
        b = b @ T_MAT
    return total


def solve_optimal_wait(belief_vec: np.ndarray, max_wait: int = 120) -> tuple[int, float]:
    """Find the wait (probe interval) minimizing (delay cost during the wait + C_PROBE at the end
    + continuation value), via a simple one-shot renewal-style search: since after a "confirmed
    good" probe the belief resets to the CONDITIONAL distribution given "not yet failed" at that
    point (a renewal), we can solve for the optimal wait via a fixed-point / average-cost search,
    analogous to the cold-side renewal-reward approach used throughout this project."""
    best_wait, best_rate = None, float("inf")
    for wait in range(1, max_wait):
        # cost of this cycle: delay cost during [0,wait) + probe cost at end
        delay_cost = expected_delay_cost_if_wait(belief_vec, wait)
        cycle_cost = delay_cost + C_PROBE
        # "cycle length" for average-cost normalization: expected real-time length is just `wait`
        # (a probe always happens exactly at `wait`, ending the cycle either by detecting Failed
        # or by confirming Good and starting the next cycle from the conditional belief)
        rate = cycle_cost / wait
        if rate < best_rate:
            best_rate, best_wait = rate, wait
    return best_wait, best_rate


def conditional_belief_given_not_failed(belief_vec: np.ndarray) -> np.ndarray:
    """Given a belief distribution (possibly including Failed), condition on NOT failed (i.e.
    the "confirmed good" probe outcome), renormalizing over phases 0..K_PHASES-1."""
    non_failed = belief_vec[:K_PHASES].copy()
    total = non_failed.sum()
    if total < 1e-12:
        return non_failed  # degenerate
    return non_failed / total


def main() -> None:
    print(f"Phase-type failure time: K={K_PHASES} phases, q={Q} -> mean T = {K_PHASES/Q:.1f} steps")
    print(f"C_PROBE={C_PROBE}, C_DELAY_PER_STEP={C_DELAY_PER_STEP}\n")

    # Start belief: phase 0 with certainty (just started / just confirmed good at t=0)
    belief = np.zeros(K_PHASES + 1)
    belief[0] = 1.0

    print("Simulating a SEQUENCE of renewal cycles (each starting from the conditional belief")
    print("after the previous 'confirmed good' probe) to see how the optimal wait (probe")
    print("interval) evolves as we move further into the schedule -- this is the direct test")
    print("of 'anticipatory probing': does interval SHRINK as expected failure time approaches?\n")

    print(f"{'cycle':>6} {'start_belief_phase_mean':>24} {'optimal_wait':>13} {'avg_rate':>10}")
    t_absolute = 0
    for cycle in range(10):
        # mean phase (a simple summary of "how far into the schedule are we, conditionally")
        phase_mean = float(np.dot(np.arange(K_PHASES + 1), belief))
        wait, rate = solve_optimal_wait(belief)
        print(f"{cycle:>6} {phase_mean:>24.3f} {wait:>13} {rate:>10.4f}  (t_absolute~{t_absolute})")

        # advance to next cycle: propagate belief forward `wait` steps, then condition on
        # "not failed" (i.e. assume the probe found it Good -- the common/expected branch,
        # useful for tracing the MOST LIKELY trajectory of optimal waits)
        propagated = survival_from_phase(belief, wait)
        belief = conditional_belief_given_not_failed(propagated)
        # pad back to full K_PHASES+1 vector with 0 in Failed slot
        full = np.zeros(K_PHASES + 1)
        full[:K_PHASES] = belief
        belief = full
        t_absolute += wait

        if belief[:K_PHASES].sum() < 1e-9:
            print("  (belief mass exhausted -- essentially certain to have failed by now)")
            break


if __name__ == "__main__":
    main()
