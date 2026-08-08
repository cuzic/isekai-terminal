"""Robustness check for the real-data Phi=0 positioning result, per opus-symbolic-advisor's
review-before-paper checklist (2026-07-19): does the "operating point sits near the Phi=0
boundary, hence low headroom for adaptive gain" finding survive perturbing cost_a and the
EM-fitted hop parameters within plausible calibration uncertainty, or is the "almost exactly on
the boundary" landing a coincidence that would evaporate under small parameter changes?

Also computes the ACTUAL adaptive gain (adaptive-optimal vs best-fixed, via
`beliefgrid2d.belief_grid2d_value_iteration_warm` -- the exact quantity `berlin_v2x_block_fit_
demo.py`'s `gain()` measures) at each perturbed point, not just Phi (fixed-vs-fixed) -- these are
DIFFERENT quantities (Phi measures how close the two fixed policies are; gain measures how much
better the true adaptive optimum is than the best of those two fixed policies), and the advisor
flagged that conflating them would be a logical gap in a paper claim.

Perturbs cost_a by +/-20%, and each hop's (p_gb, p_bg, eps_good, eps_bad) independently by +/-20%
(clipped to [0,1]), holding c_warm/c_switch_warm/c_switch_cold fixed at this project's calibrated
peak-gain operating point. Reports the range of |Phi| and gain across the perturbation box.

Run with: uv run python warm_cold_robustness_sweep_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

C_WARM = 0.005
C_SWITCH_WARM = 0.01
C_SWITCH_COLD = 0.02
RESOLUTION = 50
N_ITERS = 1500

BASE_COST_A = 0.30
BASE_HOP1 = dict(p_gb=0.1909, p_bg=0.4553, eps_good=0.0320, eps_bad=0.3010)
BASE_HOP2 = dict(p_gb=0.2764, p_bg=0.3933, eps_good=0.0695, eps_bad=0.4253)

PERTURB_FRAC = 0.20
N_SAMPLES = 80
SEED = 20260719


def clip01(x: float) -> float:
    return float(np.clip(x, 1e-4, 1 - 1e-4))


def perturb(base: float, rng: np.random.Generator, frac: float = PERTURB_FRAC) -> float:
    return base * (1.0 + rng.uniform(-frac, frac))


def evaluate(cost_a: float, hop1: channels.HopParams, hop2: channels.HopParams) -> tuple[float, float, float]:
    """Returns (Phi=g_warm-g_cold, adaptive_gain, best_fixed_g)."""
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, cost_a, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)
    sol_adapt = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_warm = switching_curves.always_warm_value_iteration(hop1, hop2, cost_a, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(hop1, hop2, cost_a, C_SWITCH_COLD, resolution=RESOLUTION, n_iters=N_ITERS)
    baseline = min(sol_warm.g, sol_cold.g)
    gain = (baseline - sol_adapt.g) / baseline if baseline > 0 else 0.0
    return sol_warm.g - sol_cold.g, gain, baseline


def main() -> None:
    rng = np.random.default_rng(SEED)

    print(f"=== Robustness sweep: cost_a and hop params perturbed +/-{PERTURB_FRAC*100:.0f}%, "
          f"n={N_SAMPLES} samples ===\n")

    # First, the unperturbed base point for reference.
    hop1_base = channels.HopParams(**BASE_HOP1)
    hop2_base = channels.HopParams(**BASE_HOP2)
    phi0, gain0, baseline0 = evaluate(BASE_COST_A, hop1_base, hop2_base)
    print(f"Base point: cost_a={BASE_COST_A}, Phi={phi0:+.6f}, |Phi|/cost_a={abs(phi0)/BASE_COST_A*100:.3f}%, "
          f"adaptive_gain={gain0*100:.3f}%\n")

    phis = []
    gains = []
    print(f"{'cost_a':>8}  {'hop1(p_gb,p_bg,eg,eb)':>34}  {'hop2(...)':>34}  {'Phi':>10}  {'gain%':>8}")
    for i in range(N_SAMPLES):
        cost_a = perturb(BASE_COST_A, rng)
        h1 = dict(BASE_HOP1)
        h2 = dict(BASE_HOP2)
        for k in h1:
            h1[k] = clip01(perturb(h1[k], rng))
        for k in h2:
            h2[k] = clip01(perturb(h2[k], rng))
        hop1 = channels.HopParams(**h1)
        hop2 = channels.HopParams(**h2)

        phi, gain_val, baseline = evaluate(cost_a, hop1, hop2)
        phis.append(phi)
        gains.append(gain_val)
        h1s = f"({h1['p_gb']:.3f},{h1['p_bg']:.3f},{h1['eps_good']:.3f},{h1['eps_bad']:.3f})"
        h2s = f"({h2['p_gb']:.3f},{h2['p_bg']:.3f},{h2['eps_good']:.3f},{h2['eps_bad']:.3f})"
        print(f"{cost_a:>8.4f}  {h1s:>34}  {h2s:>34}  {phi:>+10.5f}  {gain_val*100:>8.3f}")

    phis = np.array(phis)
    gains = np.array(gains)
    print(f"\n=== Summary across {N_SAMPLES} perturbed samples ===")
    print(f"Phi: min={phis.min():+.5f}, max={phis.max():+.5f}, mean={phis.mean():+.5f}, "
          f"|Phi| max={np.abs(phis).max():.5f}")
    print(f"adaptive gain: min={gains.min()*100:.3f}%, max={gains.max()*100:.3f}%, "
          f"mean={gains.mean()*100:.3f}%")
    print(f"fraction of samples with gain < 5%: {(gains < 0.05).mean()*100:.1f}%")
    print(f"fraction of samples with |Phi| < c_warm={C_WARM}: {(np.abs(phis) < C_WARM).mean()*100:.1f}%")


if __name__ == "__main__":
    main()
