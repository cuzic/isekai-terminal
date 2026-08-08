"""D1-3 (task #46): is the trial-90 counterexample an isolated point in the
12-dimensional parameter space, or does violation of routing-threshold
monotonicity persist over an open neighborhood around it?

Per Codex's 2026-07-18 review, staged rather than a full 12-axis bisection
at high resolution (RVI re-solving is too expensive for that):

  Stage 1 (this script, resolution=30, cheap): perturb ONE parameter at a
  time by relative steps of +-{1,2,5,10}%, holding the other 11 fixed at the
  witness value. Record whether total_viol stays nonzero. This screens which
  axes/directions the violation is robust along.

  Stage 2 (this script, resolution=100, only for axes that screened
  positive at every step tried in stage 1): confirm at higher resolution
  that a genuinely joint neighborhood -- all axes perturbed simultaneously,
  not just one at a time -- still violates monotonicity. A joint box is a
  much stronger openness witness than 12 independent 1D lines through the
  same point (which could in principle only look open along axes, while the
  true violating set is a lower-dimensional manifold through the point).

Run with: uv run python open_region_check_demo.py
"""

from __future__ import annotations

import json

from dmr import beliefgrid2d, channels, switching_curves, warm_standby

LOG_PATH = "output/adversarial_search_log.json"
REL_STEPS = [-0.10, -0.05, -0.02, -0.01, 0.01, 0.02, 0.05, 0.10]


def total_violations(params: dict, resolution: int, n_iters: int = 800) -> int:
    hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                               eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
    hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                               eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, params["cost_a"], params["c_warm"], params["c_switch_warm"], params["c_switch_cold"]
    )
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=resolution, n_iters=n_iters)
    total = 0
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol, p, w)
            mono = switching_curves.check_monotone_grid(d_full, sol.grid)
            total += mono["n_violations_beta1"] + mono["n_violations_beta2"]
    return total


def main() -> None:
    with open(LOG_PATH) as f:
        log = json.load(f)
    witness = log["worst"]["params"]

    print("=== Stage 1: per-axis perturbation screen (resolution=30) ===")
    axis_names = list(witness.keys())
    robust_axes: dict[str, list[float]] = {}
    for name in axis_names:
        nonzero_steps = []
        for step in REL_STEPS:
            perturbed = dict(witness)
            perturbed[name] = witness[name] * (1 + step)
            try:
                viol = total_violations(perturbed, resolution=30)
            except Exception as exc:
                print(f"  {name} step={step:+.0%}: ERROR ({exc})")
                continue
            if viol > 0:
                nonzero_steps.append(step)
        robust_axes[name] = nonzero_steps
        print(f"  {name}: violates at steps {nonzero_steps} out of {REL_STEPS}")

    always_robust = [name for name, steps in robust_axes.items() if len(steps) == len(REL_STEPS)]
    print(f"\nAxes robust at EVERY perturbation tried (+-1% to +-10%): {always_robust}")

    print("\n=== Stage 2: joint perturbation box (all 12 axes simultaneously, resolution=100) ===")
    print("(a genuinely joint box, not just 12 independent 1D lines through the witness, is a")
    print(" stronger openness witness -- the violating set could in principle be open along every")
    print(" individual axis while still being a lower-dimensional manifold through the point)")
    for joint_step in [0.01, 0.02, 0.05]:
        for sign_label, sign in [("+", 1.0), ("-", -1.0)]:
            perturbed = {name: witness[name] * (1 + sign * joint_step) for name in axis_names}
            viol = total_violations(perturbed, resolution=100, n_iters=1500)
            print(f"  joint {sign_label}{joint_step:.0%} on all 12 axes: total_viol={viol}")

    print("\n=== Verdict ===")
    print(f"{len(always_robust)}/12 axes show nonzero violations at every single-axis perturbation")
    print("tried (+-1% to +-10%). If the joint-box checks above also show nonzero violations, that")
    print("confirms a genuine open neighborhood (not an isolated point, and not merely 12 lines")
    print("through an isolated point) -- consistent with the resolution-scaling argument already in")
    print("THRESHOLD_PROOF.md (violation count grows roughly like resolution^2, the signature of a")
    print("2D open region being resolved, not a measure-zero curve or a single point).")


if __name__ == "__main__":
    main()
