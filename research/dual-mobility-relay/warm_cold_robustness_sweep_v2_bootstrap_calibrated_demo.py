"""SECOND robustness sweep, using empirically-justified (not asserted) perturbation widths.

The first sweep (`warm_cold_robustness_sweep_demo.py`) used a flat +/-20% on every parameter,
which the advisor suggested checking against real calibration uncertainty rather than assuming.
`berlin_v2x_bootstrap_ci_demo.py`'s block-bootstrap (8 resamples/hop, block=20 windows) found
+/-20% is NOT conservative for several parameters -- bootstrap std/point-estimate ratios:
  hop1: p_gb=14.5%, p_bg=6.3%, eps_good=14.5%, eps_bad=9.2%
  hop2: p_gb=11.7%, p_bg=7.8%, eps_good=28.8%, eps_bad=9.7%
Using ~2*std/point as a rough (quasi-normal) ~95%-CI half-width per parameter (floored at the
original 20% so this sweep is a strict widening, never a narrowing, of the first one):
  hop1: p_gb=29%, p_bg=20%(floor), eps_good=29%, eps_bad=20%(floor, actual 18.4%<20%)
  hop2: p_gb=23.4%, p_bg=20%(floor), eps_good=57.6%(!), eps_bad=20%(floor, actual 19.4%<20%)
`cost_a` itself is kept at +/-20% (its uncertainty comes from the separate real-timing
calibration in task #2/#3, not this bootstrap, and was not re-estimated here).

Re-checks whether the low-gain finding survives this wider, empirically-grounded box -- most
notably hop2's eps_good, whose uncertainty is nearly 3x the original flat +/-20%.

Run with: uv run python warm_cold_robustness_sweep_v2_bootstrap_calibrated_demo.py
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

# Bootstrap-derived per-parameter perturbation half-widths (fraction), floored at 0.20.
PERTURB_FRAC_HOP1 = dict(p_gb=0.29, p_bg=0.20, eps_good=0.29, eps_bad=0.20)
PERTURB_FRAC_HOP2 = dict(p_gb=0.234, p_bg=0.20, eps_good=0.576, eps_bad=0.20)
PERTURB_FRAC_COST_A = 0.20

N_SAMPLES = 80
SEED = 20260719


def clip01(x: float) -> float:
    return float(np.clip(x, 1e-4, 1 - 1e-4))


def perturb(base: float, rng: np.random.Generator, frac: float) -> float:
    return base * (1.0 + rng.uniform(-frac, frac))


def evaluate(cost_a: float, hop1: channels.HopParams, hop2: channels.HopParams) -> tuple[float, float, float]:
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

    print(f"=== Robustness sweep v2: bootstrap-calibrated per-parameter widths, n={N_SAMPLES} ===")
    print(f"hop1 widths: {PERTURB_FRAC_HOP1}")
    print(f"hop2 widths: {PERTURB_FRAC_HOP2}")
    print(f"cost_a width: +/-{PERTURB_FRAC_COST_A*100:.0f}%\n")

    phis, gains = [], []
    worst_gain = -1.0
    worst_params = None
    for i in range(N_SAMPLES):
        cost_a = perturb(BASE_COST_A, rng, PERTURB_FRAC_COST_A)
        h1 = dict(BASE_HOP1)
        h2 = dict(BASE_HOP2)
        for k in h1:
            h1[k] = clip01(perturb(h1[k], rng, PERTURB_FRAC_HOP1[k]))
        for k in h2:
            h2[k] = clip01(perturb(h2[k], rng, PERTURB_FRAC_HOP2[k]))
        hop1 = channels.HopParams(**h1)
        hop2 = channels.HopParams(**h2)

        phi, gain_val, baseline = evaluate(cost_a, hop1, hop2)
        phis.append(phi)
        gains.append(gain_val)
        if gain_val > worst_gain:
            worst_gain = gain_val
            worst_params = (cost_a, dict(h1), dict(h2))

    phis = np.array(phis)
    gains = np.array(gains)
    print(f"Phi: min={phis.min():+.5f}, max={phis.max():+.5f}, mean={phis.mean():+.5f}")
    print(f"adaptive gain: min={gains.min()*100:.3f}%, max={gains.max()*100:.3f}%, "
          f"mean={gains.mean()*100:.3f}%, median={np.median(gains)*100:.3f}%")
    print(f"fraction of samples with gain < 5%: {(gains < 0.05).mean()*100:.1f}%")
    print(f"fraction of samples with gain < 1%: {(gains < 0.01).mean()*100:.1f}%")
    print(f"\nworst-case (highest gain) sample: cost_a={worst_params[0]:.4f}, "
          f"hop1={worst_params[1]}, hop2={worst_params[2]}, gain={worst_gain*100:.3f}%")


if __name__ == "__main__":
    main()
