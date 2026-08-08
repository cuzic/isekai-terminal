"""Gate check (per user request, 2026-07-19): before attempting any theorem about the
always-warm vs always-cold crossover boundary, check numerically whether it's even clean
(monotone, single-crossing) across (c_switch_cold, channel persistence lambda) -- the same
kind of cheap sanity pass this project has done before committing to a proof attempt (e.g.
`voi_margin_gate_demo.py` for the single-crossing conjecture).

This is a DIFFERENT question from "is adaptive control worth it" (g_adapt vs min(g_warm,g_cold),
already measured to be small for the real Berlin V2X pair). Here we compare the two FIXED
policies directly: g_warm(c_switch_cold, lambda) vs g_cold(c_switch_cold, lambda). This is
practically relevant on its own (a system that won't build adaptive control still needs to pick
ONE fixed policy) and simpler (no warm/cold choice inside the MDP, so no action-dependent-
observability mechanism to break monotonicity the way it did for the full 4-action model).

Uses a SYMMETRIC synthetic hop pair (hop1=hop2, single controllable lambda) to isolate the
lambda dependence cleanly for this first pass -- asymmetric real-hop-pair effects can be
checked in a follow-up if this gate check looks promising.

Run with: uv run python warm_cold_boundary_gate_check_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

COST_A = 0.30  # CORRECTED: cost_a=0.16 (a leftover default) made path B's own stationary average
# loss (~0.336 for this symmetric synthetic pair, loss_good=0.05/loss_bad=0.5/pi_bad=0.3 -- checked
# directly) far worse than cost_a, degenerating BOTH always-warm and always-cold to "always route
# A, never touch B" for every c_switch_cold/lambda tested (confirmed: g_cold-g_warm was flat at
# exactly -c_warm=-0.02 everywhere, meaning routing behavior was identical and warm's only
# difference was paying c_warm for nothing -- same failure mode already seen twice with the real
# Berlin V2X pair). Raised to be comparable to path B's own stationary loss, per established
# project convention, to give the relay a genuine chance to be worth using.
C_WARM = 0.02
C_SWITCH_WARM = 0.01
RESOLUTION = 50
N_ITERS = 1500

LAMBDA_VALUES = [0.1, 0.3, 0.5, 0.55, 0.6, 0.65, 0.7, 0.8, 0.9]
C_SWITCH_COLD_VALUES = np.array([0.02, 0.05, 0.08, 0.12, 0.17, 0.23, 0.30, 0.40, 0.55, 0.75,
                                  1.00, 1.30, 1.70, 2.20, 2.80, 3.50, 4.50, 6.00, 8.00, 10.00])


def symmetric_hop(lam: float, loss_good: float = 0.05, loss_bad: float = 0.5) -> channels.HopParams:
    """A symmetric hop with a given persistence lambda=1-p_gb-p_bg, fixed stationary P(Bad)=0.3
    (so only lambda varies across calls, not the marginal loss rate)."""
    pi_bad = 0.3
    p_gb = pi_bad * (1 - lam)
    p_bg = (1 - pi_bad) * (1 - lam)
    return channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=loss_good, eps_bad=loss_bad)


def g_warm_cold(lam: float, c_switch_cold: float) -> tuple[float, float]:
    hop = symmetric_hop(lam)
    sol_warm = switching_curves.always_warm_value_iteration(hop, hop, COST_A, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(hop, hop, COST_A, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g, sol_cold.g


def find_crossover(lam: float) -> float | None:
    """Returns the c_switch_cold value where g_cold first exceeds g_warm (interpolated), i.e.
    the boundary below which always-cold wins and above which always-warm wins. None if no
    crossing is found in the swept range (one regime dominates throughout)."""
    diffs = []
    for cs in C_SWITCH_COLD_VALUES:
        g_warm, g_cold = g_warm_cold(lam, cs)
        diffs.append(g_cold - g_warm)  # >0 means cold is worse (higher cost) => warm wins
    diffs = np.array(diffs)
    print(f"  lambda={lam}: g_cold-g_warm across c_switch_cold = {diffs.round(4)}")
    sign_changes = np.where(np.diff(np.sign(diffs)) != 0)[0]
    if len(sign_changes) == 0:
        return None
    if len(sign_changes) > 1:
        print(f"    WARNING: {len(sign_changes)} sign changes found -- NOT a clean single crossing!")
    i = sign_changes[0]
    x0, x1 = C_SWITCH_COLD_VALUES[i], C_SWITCH_COLD_VALUES[i + 1]
    y0, y1 = diffs[i], diffs[i + 1]
    return x0 + (0 - y0) * (x1 - x0) / (y1 - y0)


def main() -> None:
    print(f"=== Warm-vs-cold boundary gate check (cost_a={COST_A}, c_warm={C_WARM}, c_switch_warm={C_SWITCH_WARM}) ===\n")
    crossovers = {}
    for lam in LAMBDA_VALUES:
        crossover = find_crossover(lam)
        crossovers[lam] = crossover
        print(f"  -> crossover c_switch_cold* = {crossover}\n")

    print("=== Summary: crossover c_switch_cold* vs channel persistence lambda ===")
    for lam, cs in crossovers.items():
        print(f"  lambda={lam}: c_switch_cold*={cs}")

    valid = [(l, c) for l, c in crossovers.items() if c is not None]
    if len(valid) >= 2:
        lams, css = zip(*valid)
        diffs_monotone = np.all(np.diff(css) > 0) or np.all(np.diff(css) < 0)
        print(f"\nMonotone in lambda across found crossovers: {diffs_monotone}")
    else:
        print("\nNot enough crossovers found across the swept lambda range to assess monotonicity.")


if __name__ == "__main__":
    main()
