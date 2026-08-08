"""Efficient replacement for warm_cold_boundary_gate_check_demo.py's c_switch_cold sweep, per
Fable-model review guidance (2026-07-19): since `g_cold` is provably non-decreasing (and
concave) in `c_switch_cold` -- it's a min over per-policy costs each linear/constant in
c_switch_cold -- while `g_warm` doesn't depend on c_switch_cold at all, the crossing along the
c_switch_cold axis (if it exists) is UNIQUE, and its existence is fully determined by the sign
of a single scalar:

    Phi(lambda) := min(cost_a, stationary_path_b_loss) - g_warm(lambda)

(the `c_switch_cold -> infinity` closed form for g_cold is exactly `min(cost_a,
stationary_path_b_loss)` -- no switching ever happens in that limit, verified numerically
against RVI in-session, both the closed form itself AND that the reported average cost is
ref-context-independent, i.e. no multichain artifact). Phi>0 means a crossing exists somewhere
along the c_switch_cold axis (cold-plateau exceeds warm); Phi<=0 means cold dominates warm for
EVERY c_switch_cold, no crossing exists at all -- this reframes "does lambda=0.1..0.5 lack a
crossing because the swept range was too narrow, or because none exists" into a single g_warm
computation per lambda, no c_switch_cold sweep needed.

This also lets us cheaply push lambda close to 1 to check Fable's flagged risk: VoI (hence
warm-standby's advantage) typically peaks at INTERMEDIATE persistence and can vanish again as
lambda->1 (near-frozen state needs no observation) -- the two-point (0.7, 0.9) "monotone
decreasing crossover" finding might not hold all the way to lambda=0.99.

Run with: uv run python warm_cold_phi_lambda_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, switching_curves

COST_A = 0.30
C_WARM = 0.02
C_SWITCH_WARM = 0.01
RESOLUTION = 50
N_ITERS = 1500

LAMBDA_VALUES = np.array([0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.55, 0.6, 0.62, 0.64, 0.66, 0.68,
                           0.7, 0.75, 0.8, 0.85, 0.9, 0.95, 0.97, 0.99])


def symmetric_hop(lam: float, loss_good: float = 0.05, loss_bad: float = 0.5) -> channels.HopParams:
    pi_bad = 0.3
    p_gb = pi_bad * (1 - lam)
    p_bg = (1 - pi_bad) * (1 - lam)
    return channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=loss_good, eps_bad=loss_bad)


def stationary_path_b_loss(hop: channels.HopParams) -> float:
    path_b_loss = channels.path_b_loss_prob(hop, hop)
    pi_bad = hop.p_gb / (hop.p_gb + hop.p_bg)
    pi = np.array([(1 - pi_bad) ** 2, (1 - pi_bad) * pi_bad, pi_bad * (1 - pi_bad), pi_bad ** 2])
    return float((pi * path_b_loss).sum())


def phi(lam: float) -> float:
    hop = symmetric_hop(lam)
    plateau = min(COST_A, stationary_path_b_loss(hop))
    sol_warm = switching_curves.always_warm_value_iteration(hop, hop, COST_A, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    return plateau - sol_warm.g


def main() -> None:
    print(f"=== Phi(lambda) = min(cost_a, stationary_path_b_loss) - g_warm(lambda) ===")
    print(f"(Phi>0 => a c_switch_cold crossing exists; Phi<=0 => cold dominates warm everywhere)\n")

    phis = []
    for lam in LAMBDA_VALUES:
        p = phi(lam)
        phis.append(p)
        marker = " <- CROSSING EXISTS" if p > 0 else ""
        print(f"  lambda={lam:.2f}: Phi={p:+.5f}{marker}")

    phis = np.array(phis)
    sign_changes = np.where(np.diff(np.sign(phis)) != 0)[0]
    print(f"\nsign changes in Phi(lambda) at: " +
          ", ".join(f"between {LAMBDA_VALUES[i]:.2f} and {LAMBDA_VALUES[i+1]:.2f}" for i in sign_changes)
          if len(sign_changes) else "none found")
    print(f"Phi monotone non-decreasing across the whole swept range: {np.all(np.diff(phis) >= -1e-9)}")
    print(f"max lambda tested where Phi<=0: {LAMBDA_VALUES[phis<=0].max() if np.any(phis<=0) else 'none (crosses before first point)'}")
    print(f"Phi at lambda=0.99 (near-frozen check): {phis[-1]:+.5f}")


if __name__ == "__main__":
    main()
