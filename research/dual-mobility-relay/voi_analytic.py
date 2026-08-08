"""Exact (no Monte Carlo) one-step value-of-information sweep over c_switch.

Complements sweep.py's noisy long-run Monte Carlo policy simulation with a
closed-form Bayes-risk computation (dmr/voi.py): for a single decision drawn
fresh from the channel's stationary belief, how much does knowing o_decomp
instead of o_composite reduce expected loss? Predicts a "hump" shape:
- c_switch=0: bailing to A is optimal whenever *any* single hop is Bad
  (GB, BG, BB all share the same optimal action), so the o_composite
  garbling doesn't cross a decision boundary -- gap is exactly 0.
- c_switch -> large: switching is never worth it regardless of state, so
  the optimal action stops depending on the belief at all -- gap collapses
  back to 0.
- Some intermediate c_switch: riding out a BG-only (hop1) outage while
  bailing on a GB (hop2) outage become genuinely different optimal actions,
  so distinguishing them (which composite alone can't fully do) has real
  value.

`gap(active=A)` is included for reference but is trivial by construction, not
merely a numerical coincidence: hop1/hop2 losses are only observable while
path B carries traffic (see dmr/switching.py's `simulate_belief_policy_switch`,
fixed 2026-07-17 per an external formalization review), so parked on path A
there is *no* observation at all -- composite and decomposed degenerate to
the same (empty) information, and the gap is identically zero regardless of
Q. The interesting quantity throughout is `gap(active=B)`.

Range note: under the average-cost (RVI) criterion, `gap(active=B)` rises to
a peak and then *plateaus* rather than decaying back to 0 -- it does not
retrace the discounted-planning "hump" exactly. This is a real feature of
gain-optimality, not a bug: once c_switch is large enough that the optimal
policy from active=A never switches to B (confirmed numerically -- see
STAGE0_REPORT.md), path A becomes absorbing, and the *one-time* cost of an
eventual bail-from-B (however large) gets amortized to exactly zero over an
infinite horizon -- gain-optimality is insensitive to finite transient
costs (a known gap between gain optimality and bias/Blackwell optimality;
see Puterman, "Markov Decision Processes", ch. 8-10). This degeneracy only
bites at unrealistically large c_switch; the sweep in sweep.py stays in the
realistic range (0.05-0.2) and is unaffected.

Run with: uv run python voi_analytic.py
Writes: output/voi_gap_vs_c_switch.png
"""

from __future__ import annotations

from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

from dmr import channels, switching, voi

OUTPUT_DIR = Path(__file__).parent / "output"


def main() -> None:
    OUTPUT_DIR.mkdir(exist_ok=True)

    # same "favorable for decomposition" scenario as run_stage0.py
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
    rho = 0.2
    cost_a = 0.08

    t = channels.joint_transition_matrix(hop1, hop2, rho)
    stationary = channels.stationary_distribution(t)
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    comp_lik = channels.composite_obs_likelihood(hop1, hop2)
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)

    c_switch_values = np.linspace(0.0, 10.0, 150)
    gap_active_a = np.zeros_like(c_switch_values)
    gap_active_b = np.zeros_like(c_switch_values)

    for i, cs in enumerate(c_switch_values):
        switch_cost = switching.cost_with_switching(path_b_loss, cost_a, float(cs))
        solution = switching.average_cost_value_iteration_switch(t, switch_cost)
        gap_active_a[i] = voi.decomposition_value_gap(
            stationary, comp_lik, decomp_lik, solution.q[:, 0, :]
        )
        gap_active_b[i] = voi.decomposition_value_gap(
            stationary, comp_lik, decomp_lik, solution.q[:, 1, :]
        )

    peak_idx_b = int(np.argmax(gap_active_b))
    print(f"peak gap (active=B): {gap_active_b[peak_idx_b]:.5f} at c_switch={c_switch_values[peak_idx_b]:.3f}")
    peak_idx_a = int(np.argmax(gap_active_a))
    print(f"peak gap (active=A): {gap_active_a[peak_idx_a]:.5f} at c_switch={c_switch_values[peak_idx_a]:.3f}")
    print(f"gap(active=B) at c_switch={c_switch_values[0]:.2f}:  {gap_active_b[0]:.6f}")
    print(f"gap(active=B) at c_switch={c_switch_values[-1]:.2f}:  {gap_active_b[-1]:.6f}")

    fig, ax = plt.subplots(figsize=(7, 5))
    ax.plot(c_switch_values, gap_active_a, label="currently on path A", marker=".")
    ax.plot(c_switch_values, gap_active_b, label="currently on path B", marker=".")
    ax.axhline(0, color="gray", linewidth=0.5)
    ax.set_xlabel("switching cost (c_switch)")
    ax.set_ylabel("exact one-step VoI gap: Risk(composite) - Risk(decomp)")
    ax.set_title(
        "Exact (closed-form, no Monte Carlo) decomposition value gap\nvs. switching cost"
    )
    ax.legend()
    fig.tight_layout()
    fig.savefig(OUTPUT_DIR / "voi_gap_vs_c_switch.png", dpi=150)
    print(f"wrote {OUTPUT_DIR / 'voi_gap_vs_c_switch.png'}")


if __name__ == "__main__":
    main()
