"""Task #50 (time-boxed to one check, not a standalone investigation, per
the task's own description): does a term-decomposition of the warm/cold
Q-difference explain the unexplained beta2~0.19-0.21 "wobble" in the wedge
boundary phi(beta2) (THRESHOLD_PROOF.md section 4 / paper Table 1, context
(p=A, w=cold), calibrated scenario)?

Decomposition: for the a=A sub-region, diff_m(beta) = Q(A,WARM) - Q(A,COLD)
splits into three additive terms (derived from beliefgrid2d._continuation's
observability rule -- (A,COLD) is unobservable/predict-only, (A,WARM) is
observable/Bayes-updated -- and the fact each action's continuation
bootstraps from a DIFFERENT next-context h-slice, h[:,A,WARM] vs h[:,A,COLD]):

  diff_m(beta) = c_warm                                  [immediate cost of warming]
               - voi_gap(h[:,A,WARM])(beta)               [VoI-hump term, same
                                                            mechanism as Gap G1]
               + predict_only(h[:,A,WARM] - h[:,A,COLD])(beta)   [continuation:
                                                            value of arriving warm
                                                            next step vs cold]

If the wobble tracks the VoI term's own non-monotonicity, it's the same Gap
G1 mechanism showing up again. If it tracks the continuation term instead,
it's a genuinely different (not yet documented) source. This script checks
both fields' monotonicity directly, once, and reports whichever answer it
finds -- no further chasing regardless of outcome (per the task's own
time-box).

Run with: uv run python wedge_wobble_decomposition_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, switching_curves, warm_standby
from dmr.mdp import ACTION_A
from dmr.warm_standby import ACTIONS, COLD, WARM
from invariant_features_demo import voi_gap

# The paper's calibrated/representative scenario (switching_curves_demo.py),
# same one Table 1's phi(beta2) wobble was documented against.
HOP1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
HOP2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
COST_A = 0.08
C_WARM, C_SWITCH_WARM, C_SWITCH_COLD = 0.06, 0.01, 0.5
IDX_A_COLD = ACTIONS.index((ACTION_A, COLD))
IDX_A_WARM = ACTIONS.index((ACTION_A, WARM))


def main() -> None:
    path_b_loss = channels.path_b_loss_prob(HOP1, HOP2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, C_WARM, C_SWITCH_WARM, C_SWITCH_COLD)

    resolution = 150
    sol = beliefgrid2d.belief_grid2d_value_iteration_warm(HOP1, HOP2, cost, resolution=resolution, n_iters=2000)
    grid = sol.grid

    p, w = ACTION_A, COLD  # context (p=A, w=cold), matching Table 1's caption
    diff_m_actual = sol.q[:, p, w, IDX_A_WARM] - sol.q[:, p, w, IDX_A_COLD]

    h_aw = sol.h[:, ACTION_A, WARM]
    h_ac = sol.h[:, ACTION_A, COLD]
    voi_term = -voi_gap(grid, HOP1, HOP2, h_aw)  # BayesObserved(h_aw) - PredictOnly(h_aw)

    b1_pred = beliefgrid2d.predict_scalar(grid.beta1, HOP1)
    b2_pred = beliefgrid2d.predict_scalar(grid.beta2, HOP2)
    cont_term = grid.interpolate_batch(h_aw - h_ac, b1_pred, b2_pred)

    diff_m_reconstructed = C_WARM + voi_term + cont_term
    max_reconstruction_err = float(np.max(np.abs(diff_m_reconstructed - diff_m_actual)))
    print(f"Sanity check -- decomposition algebra matches actual Q-difference: "
          f"max |reconstructed - actual| = {max_reconstruction_err:.3e} (should be ~0)")

    mono_voi = switching_curves.check_monotone_grid(voi_term, grid)
    mono_cont = switching_curves.check_monotone_grid(cont_term, grid)
    print(f"\nvoi_term monotonicity:  beta1-violations={mono_voi['n_violations_beta1']} "
          f"(max {mono_voi['max_violation_beta1']:.2e}), "
          f"beta2-violations={mono_voi['n_violations_beta2']} (max {mono_voi['max_violation_beta2']:.2e})")
    print(f"cont_term monotonicity: beta1-violations={mono_cont['n_violations_beta1']} "
          f"(max {mono_cont['max_violation_beta1']:.2e}), "
          f"beta2-violations={mono_cont['n_violations_beta2']} (max {mono_cont['max_violation_beta2']:.2e})")

    # Where exactly (in beta1) is each term non-monotone as beta2 varies, restricted
    # to the beta2 range around the documented wobble (0.19-0.21)?
    axis = grid.axis
    lo = int(np.searchsorted(axis, 0.15))
    hi = int(np.searchsorted(axis, 0.30))
    voi_grid = voi_term.reshape(grid.shape)
    cont_grid = cont_term.reshape(grid.shape)
    diff2_voi = voi_grid[:, lo:hi][:, 1:] - voi_grid[:, lo:hi][:, :-1]
    diff2_cont = cont_grid[:, lo:hi][:, 1:] - cont_grid[:, lo:hi][:, :-1]
    print(f"\nIn beta2 in [{axis[lo]:.2f}, {axis[hi]:.2f}] (the documented wobble window):")
    print(f"  voi_term beta2-direction violations in this window: {int((diff2_voi < -1e-9).sum())}")
    print(f"  cont_term beta2-direction violations in this window: {int((diff2_cont < -1e-9).sum())}")

    print("\n=== Verdict ===")
    if mono_voi["n_violations_beta2"] > mono_cont["n_violations_beta2"]:
        print("The VoI term carries the (large majority of) beta2-direction non-monotonicity --")
        print("the wedge wobble is plausibly the SAME Gap G1 VoI-hump mechanism, not a separate")
        print("phenomenon. Reported as this decomposition's answer; not investigated further per")
        print("this task's time-box.")
    elif mono_cont["n_violations_beta2"] > mono_voi["n_violations_beta2"]:
        print("The continuation term carries the (large majority of) beta2-direction non-")
        print("monotonicity -- this is a DIFFERENT mechanism from Gap G1's VoI hump (it's about the")
        print("propagated value of arriving warm vs cold next step, not about observing-before-")
        print("predicting). Reported as this decomposition's answer; not investigated further per")
        print("this task's time-box.")
    else:
        print("Both terms show comparable non-monotonicity -- inconclusive which one 'explains' the")
        print("wobble; both likely contribute. Reported as-is; not investigated further per this")
        print("task's time-box.")


if __name__ == "__main__":
    main()
