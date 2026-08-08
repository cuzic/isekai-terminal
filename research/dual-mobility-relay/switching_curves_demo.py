"""(beta1, beta2) switching-curve derivation: validates the rho=0 belief
factorization and the always-warm clamp-identity theorem, probes the full
unconstrained model's monotonicity numerically (gap G1), compares the two
models' curves (gap G2), and writes `output/switching_curves_data.json` for
the figure.

See dmr/beliefgrid2d.py and dmr/switching_curves.py module docstrings for the
underlying math, and STAGE0_REPORT.md / FORMALIZATION_REVIEW.md for the full
writeup.

Run with: uv run python switching_curves_demo.py
"""

from __future__ import annotations

import json

import numpy as np

from dmr import beliefgrid2d, beliefgrid_warm, channels, switching_curves, warm_standby


def main() -> None:
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
    cost_a = 0.08
    # This c_warm/c_switch_warm/c_switch_cold combination is the one where a
    # concrete warm/cold "wedge" (non-threshold m-structure -- corrected from
    # an earlier, unrepresentative "narrow band" reading, see
    # THRESHOLD_PROOF.md's Gap G1 section) was found during the numerical
    # G1/G2 probe -- used for the whole demo so the figure and the printed
    # diagnostics describe the same scenario.
    c_warm, c_switch_warm, c_switch_cold = 0.06, 0.01, 0.5

    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(
        path_b_loss, cost_a, c_warm, c_switch_warm, c_switch_cold
    )
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)
    t0 = channels.joint_transition_matrix(hop1, hop2, 0.0)

    print(f"scenario: hop1={hop1}, hop2={hop2}")
    print(f"cost_a={cost_a}, c_warm={c_warm}, c_switch_warm={c_switch_warm}, "
          f"c_switch_cold={c_switch_cold}\n")

    resolution = 100

    # --- 1. rho=0 factorization sanity (claim 1/2 already verified
    # separately in FORMALIZATION_REVIEW.md's follow-up; re-stated briefly
    # here for a self-contained report) ---
    print("=== Claim check: rho=0 joint transition equals kron(T1,T2) ===")
    kron_err = float(np.max(np.abs(t0 - np.kron(hop1.transition_matrix(), hop2.transition_matrix()))))
    print(f"max |T(rho=0) - kron(T1,T2)| = {kron_err:.2e}\n")

    # --- 2. 2D solve vs simplex solve (validates beliefgrid2d.py) ---
    print("=== 2D belief-MDP solve (full unconstrained model, rho=0) ===")
    sol_full = beliefgrid2d.belief_grid2d_value_iteration_warm(
        hop1, hop2, cost, resolution=resolution, n_iters=2000
    )
    sol_simplex = beliefgrid_warm.belief_grid_value_iteration_warm(
        t0, cost, decomp_lik, resolution=16, n_iters=400
    )
    print(f"2D solve (resolution={resolution}): g={sol_full.g:.6f}")
    print(f"simplex solve (resolution=16): g={sol_simplex.g:.6f} "
          f"(should be a looser/lower lower-bound; 2D reaches higher g at far less compute)\n")

    # --- 3. P1: value function monotonicity, full model ---
    print("=== P1: h(beta1,beta2,p,w) monotonicity (full unconstrained model) ===")
    p1_ok = True
    for p in range(2):
        for w in range(2):
            mono = switching_curves.check_monotone_grid(sol_full.h[:, p, w], sol_full.grid)
            viol = mono["n_violations_beta1"] + mono["n_violations_beta2"]
            p1_ok &= viol == 0
            print(f"  context p={'A' if p==0 else 'B'} w={'cold' if w==0 else 'warm'}: "
                  f"{viol} violations")
    print(f"P1 holds (h monotone in both coordinates, all contexts): {p1_ok}\n")

    # --- 4. Always-warm solve + clamp identity + P2/P3 ---
    print("=== Always-warm sub-model: clamp identity (P2/P3) ===")
    sol_aw = switching_curves.always_warm_value_iteration(
        hop1, hop2, cost_a, c_warm, c_switch_warm, resolution=resolution, n_iters=3000
    )
    clamp_err = switching_curves.verify_clamp_identity(sol_aw)
    print(f"g (always-warm) = {sol_aw.g:.6f}")
    print(f"max |Delta - clamp(d, -c_sw, c_sw)| = {clamp_err:.2e} (should be ~0, pure algebra)")
    mono_d = switching_curves.check_monotone_grid(sol_aw.d, sol_aw.grid)
    print(f"d(beta) monotonicity: {mono_d['n_violations_beta1']} + "
          f"{mono_d['n_violations_beta2']} violations (should be 0 -- this is the theorem's claim)\n")

    bail_aw = switching_curves.extract_level_curve(sol_aw.d, sol_aw.grid, c_switch_warm)
    resume_aw = switching_curves.extract_level_curve(sol_aw.d, sol_aw.grid, -c_switch_warm)
    print(f"always-warm bail curve: {len(bail_aw.beta1)} points "
          f"({len(bail_aw.no_crossing_columns)} columns with no crossing in [0,1])")
    print(f"always-warm resume curve: {len(resume_aw.beta1)} points "
          f"({len(resume_aw.no_crossing_columns)} columns with no crossing in [0,1])\n")

    # --- 5. G1: full-model d-field monotonicity probe (numerical only, no proof) ---
    print("=== G1: full-model Q-difference field monotonicity (numerical probe, no proof) ===")
    g1_ok = True
    for p in range(2):
        for w in range(2):
            d_full = switching_curves.d_field_full_model(sol_full, p, w)
            mono = switching_curves.check_monotone_grid(d_full, sol_full.grid)
            viol = mono["n_violations_beta1"] + mono["n_violations_beta2"]
            g1_ok &= viol == 0
            print(f"  context p={'A' if p==0 else 'B'} w={'cold' if w==0 else 'warm'}: "
                  f"{viol} violations")
    print(f"d_field monotone in this scenario (routing decision): {g1_ok} "
          f"-- empirically holds here; no proof backs it in general (see module docstring)\n")

    # --- 6. The predicted "warm is a band, not a threshold" structure ---
    print("=== Warm/cold (m) policy structure: threshold or band? ===")
    axis = sol_full.grid.axis
    max_m_trans, worst = 0, None
    for p_ctx in range(2):
        for w_ctx in range(2):
            for beta2 in axis:
                b2 = np.full_like(axis, beta2)
                qs = np.stack(
                    [sol_full.grid.interpolate_batch(sol_full.q[:, p_ctx, w_ctx, k], axis, b2)
                     for k in range(4)],
                    axis=1,
                )
                k_star = qs.argmin(axis=1)
                m_seq = np.array([warm_standby.ACTIONS[k][1] for k in k_star])
                m_trans = int(np.sum(m_seq[1:] != m_seq[:-1]))
                if m_trans > max_m_trans:
                    max_m_trans = m_trans
                    worst = (p_ctx, w_ctx, beta2)
    print(f"max warm<->cold transitions along any beta1 slice: {max_m_trans} "
          f"(1 = simple threshold, >1 = band structure)")
    if worst:
        print(f"worst slice: context p={'A' if worst[0]==0 else 'B'}, "
              f"w={'cold' if worst[1]==0 else 'warm'}, beta2={worst[2]:.4f}")
    print(
        "(>1 transition on a single 1D beta1 slice means warm/cold is not a threshold\n"
        "there -- this is real but scenario/slice-specific; the full 2D characterization\n"
        "(THRESHOLD_PROOF.md's Gap G1 section) is a wider 'wedge', contiguous from beta1=0\n"
        "in most beta2 rows, not a band floating in the middle everywhere)\n"
    )

    # --- 7. G2: always-warm curves vs full-model curves at the comparable (w=warm) context ---
    print("=== G2: always-warm curves vs full-model curves (context w=warm) ===")
    d_full_Bwarm = switching_curves.d_field_full_model(sol_full, 1, 1)
    d_full_Awarm = switching_curves.d_field_full_model(sol_full, 0, 1)
    bail_full = switching_curves.extract_level_curve(d_full_Bwarm, sol_full.grid, 0.0)
    resume_full = switching_curves.extract_level_curve(d_full_Awarm, sol_full.grid, 0.0)
    common_b2 = np.intersect1d(np.round(bail_aw.beta2, 6), np.round(bail_full.beta2, 6))
    if len(common_b2) > 0:
        aw_map = dict(zip(np.round(bail_aw.beta2, 6), bail_aw.beta1))
        full_map = dict(zip(np.round(bail_full.beta2, 6), bail_full.beta1))
        shifts = [full_map[b2] - aw_map[b2] for b2 in common_b2]
        print(f"bail-curve shift (full-model beta1 - always-warm beta1), "
              f"{len(shifts)} common beta2 points: mean={np.mean(shifts):.4f}, "
              f"max={np.max(shifts):.4f} -- \"the price of probing freedom\"\n")

    # --- 8. Sample trajectory (true joint belief, projected to (beta1,beta2)) ---
    print("=== Sample trajectory through belief space (for the figure) ===")
    rng = np.random.default_rng(7)
    n_c = t0.shape[0]
    stationary_start = channels.stationary_distribution(t0)
    c = int(rng.choice(n_c, p=stationary_start))
    p_ctx, w_ctx = warm_standby.ACTION_B, warm_standby.COLD
    belief = np.array([0.25, 0.25, 0.25, 0.25])
    traj_beta1, traj_beta2 = [], []
    n_steps = 400
    for step in range(n_steps):
        c = int(rng.choice(n_c, p=t0[c]))
        belief_pred = belief @ t0
        b1_pred = belief_pred[2] + belief_pred[3]
        b2_pred = belief_pred[1] + belief_pred[3]
        qs = [
            sol_full.grid.interpolate_batch(
                sol_full.q[:, p_ctx, w_ctx, k], np.array([b1_pred]), np.array([b2_pred])
            )[0]
            for k in range(4)
        ]
        k_star = int(np.argmin(qs))
        a, m = warm_standby.ACTIONS[k_star]
        observable = (a == warm_standby.ACTION_B) or (m == warm_standby.WARM)
        o = int(rng.choice(4, p=decomp_lik[c]))
        if observable:
            unnorm = belief_pred * decomp_lik[:, o]
            belief = unnorm / unnorm.sum()
        else:
            belief = belief_pred
        p_ctx, w_ctx = a, m
        if step >= 100:  # burn-in
            traj_beta1.append(b1_pred)
            traj_beta2.append(b2_pred)
    print(f"recorded {len(traj_beta1)} trajectory points (post burn-in)\n")

    # --- 9. 3-class region classification for the figure: bail (+1) /
    # hysteresis band (0) / resume (-1), on a coarser grid for a lighter SVG.
    fig_res = 60
    fig_axis = np.linspace(0.0, 1.0, fig_res + 1)
    fb1, fb2 = np.meshgrid(fig_axis, fig_axis, indexing="ij")
    fb1f, fb2f = fb1.reshape(-1), fb2.reshape(-1)

    d_aw_fig = sol_aw.grid.interpolate_batch(sol_aw.d, fb1f, fb2f)
    region_aw = np.where(d_aw_fig >= c_switch_warm, 1, np.where(d_aw_fig <= -c_switch_warm, -1, 0))

    d_full_bail_fig = sol_full.grid.interpolate_batch(d_full_Bwarm, fb1f, fb2f)
    d_full_resume_fig = sol_full.grid.interpolate_batch(d_full_Awarm, fb1f, fb2f)
    region_full = np.where(d_full_bail_fig >= 0, 1, np.where(d_full_resume_fig <= 0, -1, 0))

    # --- 10. Warm-wedge 2D region for the figure (context p=A, w=warm), on
    # the same coarse figure grid as the region classifications above ---
    qs_grid = np.stack(
        [
            sol_full.grid.interpolate_batch(
                sol_full.q[:, warm_standby.ACTION_A, warm_standby.WARM, k], fb1f, fb2f
            )
            for k in range(4)
        ],
        axis=1,
    )
    k_star_grid = qs_grid.argmin(axis=1)
    warm_mask = np.array([warm_standby.ACTIONS[k][1] for k in k_star_grid]).reshape(fb1.shape)

    data = {
        "fig_axis": fig_axis.tolist(),
        "stationary_point": [hop1.stationary_bad_prob(), hop2.stationary_bad_prob()],
        "always_warm": {
            "region": region_aw.reshape(fb1.shape).astype(int).tolist(),
            "bail_beta2": bail_aw.beta2.tolist(),
            "bail_beta1": bail_aw.beta1.tolist(),
            "resume_beta2": resume_aw.beta2.tolist(),
            "resume_beta1": resume_aw.beta1.tolist(),
        },
        "full_model": {
            "region": region_full.reshape(fb1.shape).astype(int).tolist(),
            "bail_beta2": bail_full.beta2.tolist(),
            "bail_beta1": bail_full.beta1.tolist(),
            "resume_beta2": resume_full.beta2.tolist(),
            "resume_beta1": resume_full.beta1.tolist(),
            "warm_mask": warm_mask.astype(int).tolist(),
        },
        "trajectory": {"beta1": traj_beta1, "beta2": traj_beta2},
        "scenario": {
            "cost_a": cost_a, "c_warm": c_warm,
            "c_switch_warm": c_switch_warm, "c_switch_cold": c_switch_cold,
        },
    }
    with open("output/switching_curves_data.json", "w") as f:
        json.dump(data, f)
    print("wrote output/switching_curves_data.json")


if __name__ == "__main__":
    main()
