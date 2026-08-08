"""Verification of the rho-perturbation theory (THRESHOLD_PROOF.md §7):
the exact (beta1, beta2, kappa) chart's predict-step recursion, the
first-order Bayes-update correction, and the O(rho^2) robustness of the
rho=0-optimal policy pi* when deployed at small rho>0.

Run with: uv run python rho_perturbation_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, beliefgrid_warm, channels, warm_standby


def joint_belief_from_beta_kappa(beta1: float, beta2: float, kappa: float) -> np.ndarray:
    b_gg = (1.0 - beta1) * (1.0 - beta2) + kappa
    b_gb = beta2 * (1.0 - beta1) - kappa
    b_bg = beta1 * (1.0 - beta2) - kappa
    b_bb = beta1 * beta2 + kappa
    return np.array([b_gg, b_gb, b_bg, b_bb])


def kappa_predict_closed_form(
    beta1: float, beta2: float, kappa: float, rho: float,
    hop1: channels.HopParams, hop2: channels.HopParams,
) -> float:
    lam1 = 1.0 - hop1.p_gb - hop1.p_bg
    lam2 = 1.0 - hop2.p_gb - hop2.p_bg
    q1_good, q1_bad = hop1.p_gb, 1.0 - hop1.p_bg
    q2_good, q2_bad = hop2.p_gb, 1.0 - hop2.p_bg
    m_gg, m_gb = min(q1_good, q2_good), min(q1_good, q2_bad)
    m_bg, m_bb = min(q1_bad, q2_good), min(q1_bad, q2_bad)
    m1 = m_gg - m_gb - m_bg + m_bb
    m0 = (
        (1 - beta1) * (1 - beta2) * m_gg + beta2 * (1 - beta1) * m_gb
        + beta1 * (1 - beta2) * m_bg + beta1 * beta2 * m_bb
    )
    b1n = beta1 * (1 - hop1.p_bg) + (1 - beta1) * hop1.p_gb
    b2n = beta2 * (1 - hop2.p_bg) + (1 - beta2) * hop2.p_gb
    return kappa * ((1 - rho) * lam1 * lam2 + rho * m1) + rho * (m0 - b1n * b2n)


def main() -> None:
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)

    print("=== 7.1: exact kappa predict-step recursion vs true matrix multiply ===")
    rng = np.random.default_rng(0)
    max_err = 0.0
    for _ in range(2000):
        beta1, beta2 = rng.uniform(0.05, 0.95), rng.uniform(0.05, 0.95)
        rho = rng.uniform(0.0, 1.0)
        kappa_lo = max(-beta1 * beta2, -(1 - beta1) * (1 - beta2))
        kappa_hi = min(beta1 * (1 - beta2), beta2 * (1 - beta1))
        if kappa_hi <= kappa_lo:
            continue
        kappa = rng.uniform(kappa_lo, kappa_hi)
        b = joint_belief_from_beta_kappa(beta1, beta2, kappa)
        t = channels.joint_transition_matrix(hop1, hop2, rho)
        b_pred = b @ t
        beta1n_true = b_pred[2] + b_pred[3]
        beta2n_true = b_pred[1] + b_pred[3]
        kappan_true = b_pred[3] - beta1n_true * beta2n_true
        kappan_formula = kappa_predict_closed_form(beta1, beta2, kappa, rho, hop1, hop2)
        max_err = max(max_err, abs(kappan_true - kappan_formula))
    print(f"max error over 2000 random (beta1,beta2,kappa,rho) trials: {max_err:.2e}")

    print("\n=== 7.2: O(rho^2) robustness check (exact, no Monte Carlo noise) ===")
    cost_a = 0.08
    c_warm, c_switch_warm, c_switch_cold = 0.06, 0.01, 0.5
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, cost_a, c_warm, c_switch_warm, c_switch_cold)
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)

    sol_pi_star = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=60, n_iters=1500)
    print(f"pi* solved (2D belief-MDP): g*(0) = {sol_pi_star.g:.6f}")

    def policy_fn(beliefs: np.ndarray, p: int, w: int) -> np.ndarray:
        beta1 = beliefs[:, 2] + beliefs[:, 3]
        beta2 = beliefs[:, 1] + beliefs[:, 3]
        qs = np.stack(
            [sol_pi_star.grid.interpolate_batch(sol_pi_star.q[:, p, w, k], beta1, beta2) for k in range(4)],
            axis=1,
        )
        return qs.argmin(axis=1)

    resolution = 12
    rho_values = [0.0, 0.01, 0.02, 0.04]
    results = []
    for rho in rho_values:
        t_rho = channels.joint_transition_matrix(hop1, hop2, rho)
        sol_eval = beliefgrid_warm.evaluate_fixed_policy_belief_grid_warm(
            t_rho, cost, decomp_lik, policy_fn, resolution=resolution, n_iters=800, tol=1e-9
        )
        sol_exact = beliefgrid_warm.belief_grid_value_iteration_warm(
            t_rho, cost, decomp_lik, resolution=resolution, n_iters=400, tol=1e-8
        )
        gap = sol_eval.g - sol_exact.g
        print(f"rho={rho:.3f}: g(pi*,rho)={sol_eval.g:.6f}, g*(rho)={sol_exact.g:.6f}, gap={gap:.6f}")
        results.append((rho, sol_eval.g, sol_exact.g, gap))

    base_gap = results[0][3]
    g0, g_last = results[0][2], results[-1][2]
    excess_last = results[-1][3] - base_gap
    print(f"\ng*(rho) change over the full range: {g_last - g0:.2e}")
    print(f"excess-gap growth over the same range: {excess_last:.2e}")
    print(
        "(the excess gap should stay far smaller than g*'s own change if the O(rho^2)\n"
        "robustness prediction holds -- see THRESHOLD_PROOF.md §7.2 for the honest\n"
        "caveats on what this grid resolution can and cannot distinguish)"
    )


if __name__ == "__main__":
    main()
