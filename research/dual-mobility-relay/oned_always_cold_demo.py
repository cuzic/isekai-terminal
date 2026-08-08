"""Task #62 (A0-1): does the Gap G1 obstruction (action-dependent
observability breaking routing-threshold monotonicity) already show up in
the simplest possible model -- a SINGLE relay hop (1D belief), always-cold
(no warm/cold layer)? Per Codex's 2026-07-18 review, deliberately NOT built
on a new `RegularGrid1D` class -- this is a self-contained local RVI using
`np.interp` directly, mirroring `switching_curves.always_cold_value_iteration`'s
structure but in 1D.

Two decisive outcomes (per Fable's 2026-07-18 review):
  - If 1D already breaks monotonicity: the mechanism hunt gets much easier
    (1D fields are exhaustively plottable, the 2-state HMM filter map is
    close to closed-form), and hopes for a clean single-crossing proof
    (task #64) should be tempered accordingly.
  - If 1D is provably/empirically monotone: the 2D interaction (between two
    independent hops) is ESSENTIAL to the counterexample -- Meshram,
    Manjunath & Gopalan (2018) prove threshold structure for a similar
    single-arm "observe while active" restless-bandit setting in some
    regimes, which would be consistent with this outcome.

Run with: uv run python oned_always_cold_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels
from dmr.beliefgrid2d import bayes_update_scalar, obs_prob_scalar, predict_scalar

CONTEXT_A, CONTEXT_B = 0, 1


def _base_a_b(axis: np.ndarray, hop: channels.HopParams, cost_a: float,
              content_b: np.ndarray, h: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    h_a, h_b = h[:, CONTEXT_A], h[:, CONTEXT_B]
    b_pred_a = predict_scalar(axis, hop)
    cont_a = np.interp(b_pred_a, axis, h_a)  # A is blind: deterministic predict only

    cont_b = np.zeros(len(axis))
    for loss in (0, 1):
        p_l = obs_prob_scalar(axis, hop, loss)
        b_next = predict_scalar(bayes_update_scalar(axis, hop, loss), hop)
        cont_b += p_l * np.interp(b_next, axis, h_b)  # B is observable: Bayes update

    return cost_a + cont_a, content_b + cont_b


def solve_1d_always_cold(hop: channels.HopParams, cost_a: float, c_switch: float,
                          resolution: int = 200, n_iters: int = 3000, tol: float = 1e-9):
    """Mirrors `switching_curves.always_cold_value_iteration` in 1D: 2
    routing actions (A blind/direct, B observable/relay), context = current
    active route, no warm/cold layer at all.

    FIXED per Codex review (2026-07-18): the original version computed `d`
    from the last loop iteration's `base_a`/`base_b`, one Bellman backup
    stale relative to the final converged `h` -- matching
    `switching_curves.always_cold_value_iteration`'s own pattern of
    recomputing `base_a`/`base_b` fresh from the final `h` after the loop,
    not reusing whatever the loop body last computed. Also now returns
    `converged`/`n_iters_used` so callers can tell a false-negative
    (violation missed because the solve didn't converge) from a genuine
    negative result."""
    axis = np.linspace(0.0, 1.0, resolution + 1)
    content_b = obs_prob_scalar(axis, hop, loss=1)

    ref_index = int(np.argmin(np.abs(axis - 0.5)))
    h = np.zeros((resolution + 1, 2))
    g = 0.0
    converged = False
    n_iters_used = n_iters
    for it in range(n_iters):
        base_a, base_b = _base_a_b(axis, hop, cost_a, content_b, h)
        q_context_a = np.stack([base_a, base_b + c_switch], axis=1)
        q_context_b = np.stack([base_a + c_switch, base_b], axis=1)
        h_full = np.stack([q_context_a.min(axis=1), q_context_b.min(axis=1)], axis=1)

        g_new = float(h_full[ref_index, CONTEXT_A])
        h_new = h_full - g_new
        converged = np.max(np.abs(h_new - h)) < tol and abs(g_new - g) < tol
        h, g = h_new, g_new
        if converged:
            n_iters_used = it + 1
            break

    base_a, base_b = _base_a_b(axis, hop, cost_a, content_b, h)  # recompute from FINAL h, not stale
    d = base_b - base_a  # context-independent, same construction as the 2D model
    return axis, d, g, converged, n_iters_used


def check_monotone_1d(d: np.ndarray, tol: float = 1e-9) -> tuple[int, float]:
    diffs = np.diff(d)
    viol = diffs < -tol
    n_viol = int(viol.sum())
    max_viol = float(-diffs[viol].min()) if n_viol else 0.0
    return n_viol, max_viol


def random_scenario(rng: np.random.Generator) -> dict:
    """Sampling ranges matched EXACTLY to `always_cold_adversarial_search_demo.py`'s
    `random_scenario` (per-hop ranges applied to this model's single hop) --
    fixed per Codex review (2026-07-18), which caught that the original
    version used a narrower `c_switch` range (max ~0.5 vs. the 2D search's
    max 1.0) and a different `p_gb` range, making the "0/250" result not
    comparable to the 2D search it was meant to contrast against."""
    p_gb = 10 ** rng.uniform(-2.5, -0.7)
    p_bg = 10 ** rng.uniform(-1.5, -0.1)
    eps_good = rng.uniform(0.001, 0.05)
    eps_bad = rng.uniform(eps_good + 0.02, 0.95)
    cost_a = rng.uniform(0.02, 0.3)
    c_switch = 10 ** rng.uniform(-2, 0)
    return dict(p_gb=p_gb, p_bg=p_bg, eps_good=eps_good, eps_bad=eps_bad, cost_a=cost_a, c_switch=c_switch)


def main() -> None:
    print("=== Sanity check: representative scenario ===")
    hop = channels.HopParams(p_gb=0.05, p_bg=0.4, eps_good=0.01, eps_bad=0.6)
    axis, d, g, converged, n_iters_used = solve_1d_always_cold(hop, cost_a=0.1, c_switch=0.05,
                                                                resolution=200, n_iters=3000)
    n_viol, max_viol = check_monotone_1d(d)
    print(f"g={g:.6f}, converged={converged} ({n_iters_used} iters), "
          f"monotonicity violations: {n_viol} (max {max_viol:.4e})")

    print("\n=== Random adversarial search: 250 scenarios, resolution=100 ===")
    print("(sampling ranges matched exactly to always_cold_adversarial_search_demo.py's, per Codex review)")
    rng = np.random.default_rng(13)
    n_trials = 250
    worst = {"n_viol": 0, "max_viol": 0.0, "params": None}
    n_violators = 0
    n_not_converged = 0
    trial_min_diffs = []  # (min_diff, params) -- margin even when non-violating
    for trial in range(n_trials):
        p = random_scenario(rng)
        hop = channels.HopParams(p_gb=p["p_gb"], p_bg=p["p_bg"], eps_good=p["eps_good"], eps_bad=p["eps_bad"])
        _, d, _, converged, _ = solve_1d_always_cold(hop, p["cost_a"], p["c_switch"], resolution=100, n_iters=3000)
        if not converged:
            n_not_converged += 1
        n_viol, max_viol = check_monotone_1d(d)
        min_diff = float(np.min(np.diff(d)))
        trial_min_diffs.append((min_diff, p))
        if n_viol > 0:
            n_violators += 1
        if n_viol > worst["n_viol"]:
            worst = {"n_viol": n_viol, "max_viol": max_viol, "params": p}
            print(f"trial {trial}: new worst = {n_viol} violations, max {max_viol:.4e}")

    print(f"\nprevalence: {n_violators}/{n_trials} scenarios show a 1D monotonicity violation")
    print(f"worst case: {worst['n_viol']} violations, magnitude {worst['max_viol']:.4e}")
    print(f"convergence: {n_trials - n_not_converged}/{n_trials} trials converged within n_iters=3000")

    trial_min_diffs.sort(key=lambda t: t[0])
    print("\n=== Robustness check: re-solve the 10 CLOSEST-TO-VIOLATING trials at 4x resolution ===")
    print("(the 0-violation trials at resolution=100 could still hide a violation between grid")
    print(" points -- re-checking the trials with the smallest positive margin at finer resolution")
    print(" is a targeted way to probe for that, cheaper than re-running all 250 at high resolution)")
    any_flipped = False
    for min_diff, p in trial_min_diffs[:10]:
        hop = channels.HopParams(p_gb=p["p_gb"], p_bg=p["p_bg"], eps_good=p["eps_good"], eps_bad=p["eps_bad"])
        _, d_fine, _, conv_fine, _ = solve_1d_always_cold(hop, p["cost_a"], p["c_switch"],
                                                           resolution=400, n_iters=4000)
        n_viol_fine, max_viol_fine = check_monotone_1d(d_fine)
        flipped = n_viol_fine > 0
        any_flipped = any_flipped or flipped
        print(f"  res=100 min_diff={min_diff:.4e} -> res=400: violations={n_viol_fine} "
              f"(max {max_viol_fine:.4e}){' <-- FLIPPED TO VIOLATING' if flipped else ''}")

    if worst["n_viol"] == 0 and not any_flipped:
        print("\n=== Verdict ===")
        print("The 1D single-hop always-cold model shows NO monotonicity violation across 250 random")
        print("scenarios at resolution=100, and none of the 10 closest-to-violating trials flip to")
        print("violating at 4x resolution either. This is empirical evidence (250+10 solves, not a")
        print("proof) consistent with the 2D interaction between two independent hops being ESSENTIAL")
        print("to Gap G1's counterexample -- a single hop's action-dependent observability alone did")
        print("not produce a violation anywhere searched.")
        return

    print(f"\nworst params: {worst['params']}")
    print("\n=== Resolution-convergence check on the worst case ===")
    hop = channels.HopParams(p_gb=worst["params"]["p_gb"], p_bg=worst["params"]["p_bg"],
                              eps_good=worst["params"]["eps_good"], eps_bad=worst["params"]["eps_bad"])
    for resolution in [100, 200, 400, 800]:
        _, d, _, _, _ = solve_1d_always_cold(hop, worst["params"]["cost_a"], worst["params"]["c_switch"],
                                              resolution=resolution, n_iters=4000)
        n_viol, max_viol = check_monotone_1d(d)
        print(f"resolution={resolution:>4}: violations={n_viol:>5}, magnitude={max_viol:.4e}, "
              f"magnitude*resolution={max_viol * resolution:.4f}")

    print("\n=== Verdict ===")
    print("The 1D single-hop always-cold model DOES show a monotonicity violation -- Gap G1's")
    print("obstruction does NOT require 2D/multi-hop interaction; a single action-dependent-")
    print("observability hop is already sufficient. This should temper hopes for a clean")
    print("single-crossing proof (task #64) -- if even the 1D case can break, the 2D full model")
    print("is unlikely to be easier.")


if __name__ == "__main__":
    main()
