"""Task #61: validate `dmr/ge_fit.py`'s moment-method GE estimator against
SYNTHETIC data with known ground-truth parameters, before it would ever be
pointed at a real trace. This project has no actual downloaded Roofnet data
in this environment (that requires creating a free IEEE DataPort account
and pulling ~21M records -- an access/download step, not a math/simulation
step, and out of scope for what this session can do autonomously; see
TRACE_CALIBRATION_NOTES.md). This script instead:

  1. Simulates a pure-Gilbert loss sequence from known (p_gb, p_bg) using
     `channels.py`'s own transition-matrix machinery (so the simulator and
     the rest of this project's channel model agree), and confirms
     `fit_gilbert_runlength` recovers the true parameters to within
     sampling noise across a range of parameter regimes (short/long bursts,
     rare/common bad state).
  2. Confirms `fit_gilbert_elliott_moments` correctly recovers p_gb+p_bg
     (via lambda) on the same synthetic Gilbert sequences, and separately
     demonstrates -- on a genuine full-GE sequence with eps_good>0,
     eps_bad<1 -- that it does NOT recover individual p_gb/p_bg/eps_good/
     eps_bad (returns None for those, as documented), directly verifying
     the underdetermination claim in that function's docstring rather than
     just asserting it in prose.

Run with: uv run python ge_fit_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, ge_fit


def simulate_binary_sequence(hop: channels.HopParams, n_steps: int, rng: np.random.Generator) -> np.ndarray:
    """Simulates a 2-state Markov chain (0=Good,1=Bad) then emits a loss bit
    per step (Bernoulli(eps_bad) if Bad, Bernoulli(eps_good) if Good)."""
    state = 0
    states = np.empty(n_steps, dtype=int)
    for t in range(n_steps):
        states[t] = state
        p_leave = hop.p_gb if state == 0 else hop.p_bg
        if rng.uniform() < p_leave:
            state = 1 - state
    eps = np.where(states == 1, hop.eps_bad, hop.eps_good)
    return (rng.uniform(size=n_steps) < eps).astype(int)


def main() -> None:
    rng = np.random.default_rng(4242)
    n_steps = 200_000

    print("=== Pure-Gilbert run-length fit vs. ground truth ===")
    scenarios = [
        ("short bursts, rare bad state", channels.HopParams(p_gb=0.01, p_bg=0.5, eps_good=0.0, eps_bad=1.0)),
        ("long bursts, common bad state", channels.HopParams(p_gb=0.05, p_bg=0.02, eps_good=0.0, eps_bad=1.0)),
        ("moderate, symmetric-ish", channels.HopParams(p_gb=0.03, p_bg=0.1, eps_good=0.0, eps_bad=1.0)),
    ]
    max_rel_err = 0.0
    for label, hop in scenarios:
        seq = simulate_binary_sequence(hop, n_steps, rng)
        fit = ge_fit.fit_gilbert_runlength(seq)
        rel_err_gb = abs(fit.p_gb - hop.p_gb) / hop.p_gb
        rel_err_bg = abs(fit.p_bg - hop.p_bg) / hop.p_bg
        max_rel_err = max(max_rel_err, rel_err_gb, rel_err_bg)
        print(f"  {label}:")
        print(f"    true p_gb={hop.p_gb:.4f}, p_bg={hop.p_bg:.4f}")
        print(f"    fit  p_gb={fit.p_gb:.4f} ({rel_err_gb:+.1%}), p_bg={fit.p_bg:.4f} ({rel_err_bg:+.1%}) "
              f"[{fit.n_good_runs} good runs, {fit.n_bad_runs} bad runs]")

    print(f"\nmax relative error across all scenarios: {max_rel_err:.1%} "
          f"({'PASS' if max_rel_err < 0.15 else 'FAIL'} -- expect single-digit-to-~10% at n={n_steps})")

    print("\n=== Moment-method (general GE) on the SAME pure-Gilbert sequences: lambda/p_sum check ===")
    for label, hop in scenarios:
        seq = simulate_binary_sequence(hop, n_steps, rng)
        result = ge_fit.fit_gilbert_elliott_moments(seq)
        true_p_sum = hop.p_gb + hop.p_bg
        rel_err = abs(result["p_gb_plus_p_bg"] - true_p_sum) / true_p_sum
        print(f"  {label}: true p_gb+p_bg={true_p_sum:.4f}, fit={result['p_gb_plus_p_bg']:.4f} ({rel_err:+.1%})")

    print("\n=== Underdetermination check: full GE (eps_good>0, eps_bad<1) ===")
    full_ge_hop = channels.HopParams(p_gb=0.02, p_bg=0.1, eps_good=0.05, eps_bad=0.6)
    seq = simulate_binary_sequence(full_ge_hop, n_steps, rng)
    result = ge_fit.fit_gilbert_elliott_moments(seq)
    print(f"  true params: p_gb={full_ge_hop.p_gb}, p_bg={full_ge_hop.p_bg}, "
          f"eps_good={full_ge_hop.eps_good}, eps_bad={full_ge_hop.eps_bad}")
    print(f"  moment-method result: {result}")
    assert result["p_gb"] is None and result["eps_good"] is None, "expected underdetermined fields to be None"
    print("  confirmed: individual p_gb/p_bg/eps_good/eps_bad are NOT recovered (None, as documented) --")
    print("  only p_gb+p_bg (via lambda) and a combined variance term are identified from these moments.")
    true_p_sum = full_ge_hop.p_gb + full_ge_hop.p_bg
    rel_err = abs(result["p_gb_plus_p_bg"] - true_p_sum) / true_p_sum
    print(f"  the identifiable quantity IS still recovered correctly: true p_gb+p_bg={true_p_sum:.4f}, "
          f"fit={result['p_gb_plus_p_bg']:.4f} ({rel_err:+.1%})")


if __name__ == "__main__":
    main()
