"""Stage 0 parameter sweep: characterize where hop decomposition has value.

Uses the switching-cost-augmented routing model (dmr/switching.py), not the
plain no-switching-cost model in dmr/mdp.py: run_stage0.py demonstrated that
without a switching cost, the optimal action in a single-hop-bad state is
always "bail to A" regardless of which hop failed, so decomposition has zero
*decision* value even though it has positive mutual information. Only once
switching carries a real cost does the plan's hypothesized asymmetry
(bail immediately on hop2, ride out transient hop1) have anything to bite on.

Sweeps inter-hop correlation (rho), hop2's mean bad-burst length, and the
switching cost c_switch (hop1 held fixed: short bursts, moderate loss --
the "drone<->car obstruction" case that favors decomposition). For each grid
point computes:

- MI gap: I(X; O_decomp) - I(X; O_composite), in bits.
- Policy value gap: composite-belief-policy average cost minus
  hop-decomposed-belief-policy average cost (the realized routing benefit),
  from Monte Carlo simulation of the QMDP belief policy under each
  observation model.
- Max possible gap: min(naive always-A, naive always-B) cost minus oracle
  (full channel-state-observed) cost, i.e. the ceiling any information could
  buy -- lets us read the policy gap as a fraction of what's achievable.

Run with: uv run python sweep.py
Writes: output/mi_gap_heatmap.png, output/policy_gap_heatmap_c_switch_*.png,
        output/sweep_results.csv
"""

from __future__ import annotations

import csv
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

from dmr import channels, information, mdp, switching

OUTPUT_DIR = Path(__file__).parent / "output"


def run_grid_point(
    hop1: channels.HopParams,
    hop2: channels.HopParams,
    rho: float,
    cost_a: float,
    c_switch: float,
    n_traj: int,
    n_steps: int,
    burn_in: int,
    seed: int,
) -> dict:
    t = channels.joint_transition_matrix(hop1, hop2, rho)
    stationary = channels.stationary_distribution(t)

    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    switch_cost = switching.cost_with_switching(path_b_loss, cost_a, c_switch)
    solution = switching.average_cost_value_iteration_switch(t, switch_cost)

    comp_lik = channels.composite_obs_likelihood(hop1, hop2)
    decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)
    mi_composite = information.mutual_information(stationary, comp_lik)
    mi_decomp = information.mutual_information(stationary, decomp_lik)

    oracle_cost = switching.induced_chain_avg_cost(t, switch_cost, solution.policy)
    always_a_cost = switching.induced_chain_avg_cost(
        t, switch_cost, switching.constant_policy(4, mdp.ACTION_A)
    )
    always_b_cost = switching.induced_chain_avg_cost(
        t, switch_cost, switching.constant_policy(4, mdp.ACTION_B)
    )
    max_possible_gap = min(always_a_cost, always_b_cost) - oracle_cost

    result_composite = switching.simulate_belief_policy_switch(
        t, comp_lik, switch_cost, solution, n_traj, n_steps, burn_in, seed=seed
    )
    result_decomp = switching.simulate_belief_policy_switch(
        t, decomp_lik, switch_cost, solution, n_traj, n_steps, burn_in, seed=seed + 1
    )
    policy_gap = result_composite.mean_cost - result_decomp.mean_cost
    # combined stderr of the difference of two independent MC estimates
    policy_gap_stderr = float(
        np.sqrt(result_composite.stderr_cost**2 + result_decomp.stderr_cost**2)
    )

    return {
        "rho": rho,
        "c_switch": c_switch,
        "mi_composite": mi_composite,
        "mi_decomp": mi_decomp,
        "mi_gap": mi_decomp - mi_composite,
        "oracle_cost": oracle_cost,
        "always_a_cost": always_a_cost,
        "always_b_cost": always_b_cost,
        "max_possible_gap": max_possible_gap,
        "policy_gap": policy_gap,
        "policy_gap_stderr": policy_gap_stderr,
        "policy_gap_frac_of_max": policy_gap / max_possible_gap if max_possible_gap > 1e-9 else 0.0,
    }


def main() -> None:
    OUTPUT_DIR.mkdir(exist_ok=True)

    # hop1: drone<->car UHF/Wi-Fi -- short, moderate-severity bad bursts
    # (the "favorable for decomposition" scenario from run_stage0.py).
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    cost_a = 0.08
    n_traj, n_steps, burn_in = 120, 700, 200

    rhos = np.linspace(0.0, 0.9, 6)
    # hop2 mean bad burst length varied via p_bg (1/p_bg = mean burst length)
    burst_lengths = np.array([2, 5, 10, 20, 40])
    p_bg_values = 1.0 / burst_lengths
    eps_bad2 = 0.6  # hop2 bad-state loss rate held fixed in this grid
    p_gb2 = 0.02  # hop2 stationary P(bad) held roughly fixed across the burst sweep
    c_switch_values = np.array([0.05, 0.1, 0.2])

    rows = []
    mi_gap_grid = np.zeros((len(burst_lengths), len(rhos)))
    policy_gap_frac_grids = {
        cs: np.zeros((len(burst_lengths), len(rhos))) for cs in c_switch_values
    }

    for bi, p_bg in enumerate(p_bg_values):
        hop2 = channels.HopParams(p_gb=p_gb2, p_bg=p_bg, eps_good=0.01, eps_bad=eps_bad2)
        for ri, rho in enumerate(rhos):
            # MI gap doesn't depend on c_switch, compute once per (burst, rho)
            t = channels.joint_transition_matrix(hop1, hop2, float(rho))
            stationary = channels.stationary_distribution(t)
            comp_lik = channels.composite_obs_likelihood(hop1, hop2)
            decomp_lik = channels.decomposed_obs_likelihood(hop1, hop2)
            mi_gap = information.mutual_information(
                stationary, decomp_lik
            ) - information.mutual_information(stationary, comp_lik)
            mi_gap_grid[bi, ri] = mi_gap

            for cs in c_switch_values:
                res = run_grid_point(
                    hop1, hop2, float(rho), cost_a, float(cs),
                    n_traj, n_steps, burn_in, seed=1000 * bi + 10 * ri + int(cs * 100),
                )
                res["hop2_burst_length"] = float(burst_lengths[bi])
                rows.append(res)
                policy_gap_frac_grids[cs][bi, ri] = res["policy_gap_frac_of_max"]
                print(
                    f"burst={burst_lengths[bi]:>3.0f} rho={rho:.2f} c_switch={cs:.2f} "
                    f"MI_gap={mi_gap:.4f} bits  "
                    f"policy_gap={res['policy_gap']:.4f}+/-{res['policy_gap_stderr']:.4f} "
                    f"({res['policy_gap_frac_of_max']*100:.1f}% of max {res['max_possible_gap']:.4f})"
                )

    with open(OUTPUT_DIR / "sweep_results.csv", "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        writer.writerows(rows)

    _plot_heatmap(
        mi_gap_grid, rhos, burst_lengths,
        title="Information gain from hop decomposition\nI(X;O_decomp) - I(X;O_composite) [bits]",
        out_path=OUTPUT_DIR / "mi_gap_heatmap.png",
    )
    for cs, grid in policy_gap_frac_grids.items():
        _plot_heatmap(
            grid, rhos, burst_lengths,
            title=f"Routing-policy value gain from hop decomposition (c_switch={cs:.2f})\n"
                  "(fraction of oracle-vs-naive ceiling)",
            out_path=OUTPUT_DIR / f"policy_gap_heatmap_c_switch_{cs:.2f}.png",
            fmt="{:.0%}",
        )
    print(f"\nwrote {OUTPUT_DIR / 'sweep_results.csv'}")
    print(f"wrote {OUTPUT_DIR / 'mi_gap_heatmap.png'}")
    for cs in c_switch_values:
        print(f"wrote {OUTPUT_DIR / f'policy_gap_heatmap_c_switch_{cs:.2f}.png'}")


def _plot_heatmap(grid, rhos, burst_lengths, title, out_path, fmt="{:.3f}"):
    fig, ax = plt.subplots(figsize=(7, 5))
    im = ax.imshow(grid, aspect="auto", origin="lower", cmap="viridis")
    ax.set_xticks(range(len(rhos)))
    ax.set_xticklabels([f"{r:.1f}" for r in rhos])
    ax.set_yticks(range(len(burst_lengths)))
    ax.set_yticklabels([f"{b:.0f}" for b in burst_lengths])
    ax.set_xlabel("inter-hop correlation (rho)")
    ax.set_ylabel("hop2 mean bad-burst length (steps)")
    ax.set_title(title)
    for bi in range(grid.shape[0]):
        for ri in range(grid.shape[1]):
            ax.text(ri, bi, fmt.format(grid[bi, ri]), ha="center", va="center",
                     color="white" if grid[bi, ri] < grid.max() * 0.6 else "black", fontsize=8)
    fig.colorbar(im, ax=ax)
    fig.tight_layout()
    fig.savefig(out_path, dpi=150)
    plt.close(fig)


if __name__ == "__main__":
    main()
