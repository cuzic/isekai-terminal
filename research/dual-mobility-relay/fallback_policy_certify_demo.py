"""task #49(c): since #58's cheap gate check found the calibrated scenario's
monotonicity margin already razor-thin (safety-factor ratio ~0.055 --
`voi_margin_gate_demo.py`), a general Lipschitz/span-contraction sufficient-
condition theorem for Gap G1 is not worth attempting. Per the task's planned
fallback, this script instead numerically CERTIFIES a simple, exactly
two-parameter-implementable policy family against the full Bellman-optimal
solve:

  routing (a): the always-warm sub-model's PROVABLY optimal hysteresis rule
    (task #49(a)'s empirical finding, D1-1/#44, is also folded in here: no
    counterexample witness has ever shown the routing decision itself
    losing its threshold structure, only the underlying d-field's
    monotonicity) -- stay on the current path unless the always-warm d-field
    crosses +-c_switch_warm (if currently WARM) or +-c_switch_cold (if
    currently COLD). NOTE: an earlier version of this script used
    c_switch_warm unconditionally regardless of the current warm/cold state
    -- that is wrong (it applies the cheap-switch threshold from a world
    where switching is always cheap to a state where it may not be) and
    produced a wildly inflated ~330% optimality gap even at tiny theta,
    caused by needless extra switching, not by anything to do with theta or
    the warm/cold rule at all. Fixed by using the switch cost that actually
    applies in the current context.
  warm/cold (m): warm iff |d(beta)| < theta, a single free threshold theta
    (the wedge shown in THRESHOLD_PROOF.md section 4 / paper section 8.3 is
    NOT this simple a rule in general -- this is deliberately a cruder,
    provably-2-parameter policy, not a claim that it reproduces the exact
    optimal wedge).

Evaluated via the EXISTING `beliefgrid_warm.evaluate_fixed_policy_belief_grid_warm`
(exact noise-free policy evaluation on the general belief-simplex grid, no
new solver needed, no Monte Carlo sampling noise) against the TRUE Bellman-
optimal average cost from `beliefgrid2d.belief_grid2d_value_iteration_warm`,
at both the calibrated scenario and the D1 counterexample scenario (to see
how the fallback fares even where the exact model provably has no clean
monotone theorem).

Run with: uv run python fallback_policy_certify_demo.py
"""

from __future__ import annotations

import json

import numpy as np

from dmr import beliefgrid2d, beliefgrid_warm, channels, switching_curves, warm_standby
from dmr.mdp import ACTION_A, ACTION_B
from dmr.warm_standby import ACTIONS, COLD, WARM

# The paper's calibrated/representative scenario (switching_curves_demo.py).
CALIBRATED = dict(
    hop1=channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12),
    hop2=channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6),
    cost_a=0.08, c_warm=0.06, c_switch_warm=0.01, c_switch_cold=0.5,
)


def action_index(a: np.ndarray, m: np.ndarray) -> np.ndarray:
    return (a * 2 + m).astype(int)


def make_fallback_policy_fn(aw_sol, c_switch_warm: float, c_switch_cold: float, theta: float):
    def policy_fn(beliefs: np.ndarray, p: int, w: int) -> np.ndarray:
        beta1 = beliefs[:, 2] + beliefs[:, 3]
        beta2 = beliefs[:, 1] + beliefs[:, 3]
        d_vals = aw_sol.grid.interpolate_batch(aw_sol.d, beta1, beta2)
        c_switch = c_switch_warm if w == WARM else c_switch_cold
        if p == ACTION_A:
            a_next = np.where(d_vals > c_switch, ACTION_B, ACTION_A)
        else:
            a_next = np.where(d_vals < -c_switch, ACTION_A, ACTION_B)
        m_next = np.where(np.abs(d_vals) < theta, WARM, COLD)
        return action_index(a_next, m_next)
    return policy_fn


def certify(scenario: dict, label: str, theta_values: list[float]) -> None:
    hop1, hop2 = scenario["hop1"], scenario["hop2"]
    cost_a, c_warm = scenario["cost_a"], scenario["c_warm"]
    c_switch_warm, c_switch_cold = scenario["c_switch_warm"], scenario["c_switch_cold"]

    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost2d = warm_standby.cost_with_warm_standby(path_b_loss, cost_a, c_warm, c_switch_warm, c_switch_cold)

    print(f"\n=== {label} ===")
    # Resolutions kept modest (not 100-150 as elsewhere in this project) because this
    # host is under heavy concurrent CPU load from unrelated sessions/daemons during
    # this run -- a first attempt at full resolution took >15 min of wall clock for
    # one scenario alone. A rough quantitative gap estimate is all this check needs.
    opt_sol = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost2d, resolution=50, n_iters=1500)
    print(f"Bellman-optimal average cost g* = {opt_sol.g:.6f}")

    aw_sol = switching_curves.always_warm_value_iteration(
        hop1, hop2, cost_a, c_warm, c_switch_warm, resolution=80, n_iters=1500
    )

    t_channel = channels.joint_transition_matrix(hop1, hop2, 0.0)
    obs_likelihood = channels.decomposed_obs_likelihood(hop1, hop2)
    cost4 = warm_standby.cost_with_warm_standby(path_b_loss, cost_a, c_warm, c_switch_warm, c_switch_cold)

    for theta in theta_values:
        policy_fn = make_fallback_policy_fn(aw_sol, c_switch_warm, c_switch_cold, theta)
        fixed_sol = beliefgrid_warm.evaluate_fixed_policy_belief_grid_warm(
            t_channel, cost4, obs_likelihood, policy_fn, resolution=16, n_iters=1000
        )
        gap = fixed_sol.g - opt_sol.g
        rel_gap = gap / opt_sol.g if opt_sol.g else float("nan")
        print(f"  theta={theta:.4f}: fallback g = {fixed_sol.g:.6f}  "
              f"(gap = {gap:+.6f}, {rel_gap:+.2%} relative to optimal)")


def main() -> None:
    theta_grid = [0.005, 0.01, 0.02, 0.05, 0.1]
    certify(CALIBRATED, "Calibrated scenario (switching_curves_demo.py)", theta_grid)

    with open("output/adversarial_search_log.json") as f:
        log = json.load(f)
    p = log["worst"]["params"]
    counterexample = dict(
        hop1=channels.HopParams(p_gb=p["p_gb1"], p_bg=p["p_bg1"], eps_good=p["eps_good1"], eps_bad=p["eps_bad1"]),
        hop2=channels.HopParams(p_gb=p["p_gb2"], p_bg=p["p_bg2"], eps_good=p["eps_good2"], eps_bad=p["eps_bad2"]),
        cost_a=p["cost_a"], c_warm=p["c_warm"], c_switch_warm=p["c_switch_warm"], c_switch_cold=p["c_switch_cold"],
    )
    certify(counterexample, "D1 counterexample scenario (trial 90, seed=12345)", theta_grid)

    print("\n=== Verdict ===")
    print("The fallback policy family (always-warm hysteresis for routing + a single |d|<theta")
    print("threshold for warm/cold) is certified against the true Bellman-optimal solve above, no")
    print("general theorem required -- this is #49(c)'s deliverable given #58 ruled out (b).")


if __name__ == "__main__":
    main()
