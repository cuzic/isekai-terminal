"""Verification of the n-hop generalization (dmr/nhop.py): the rho=0 belief
factorization and the always-warm clamp theorem, both generalized from 2
hops to n hops for a single serially-composed relay path with a binary
routing choice (direct path vs. the n-hop relay). See THRESHOLD_PROOF.md §6
for the full derivation, scope, and citations.

Run with: uv run python nhop_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, nhop


def main() -> None:
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.5, eps_good=0.01, eps_bad=0.12)
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.05, eps_good=0.01, eps_bad=0.6)
    hop3 = channels.HopParams(p_gb=0.03, p_bg=0.2, eps_good=0.02, eps_bad=0.35)

    print("=== Consistency check: n=2 nhop.py must match channels.py exactly ===")
    t_nhop = nhop.joint_transition_matrix_nhop([hop1, hop2])
    t_existing = channels.joint_transition_matrix(hop1, hop2, rho=0.0)
    print(f"transition matrix max diff: {np.max(np.abs(t_nhop - t_existing)):.2e}")
    lik_nhop = nhop.decomposed_obs_likelihood_nhop([hop1, hop2])
    lik_existing = channels.decomposed_obs_likelihood(hop1, hop2)
    print(f"obs likelihood max diff:    {np.max(np.abs(lik_nhop - lik_existing)):.2e}")

    print("\n=== Proposition 1': n=3 belief factorization ===")
    err = nhop.verify_factorization_nhop([hop1, hop2, hop3], n_steps=10, seed=1)
    print(f"max factorization error over 10 random predict+update steps: {err:.2e}")

    print("\n=== Proposition 2'/3': n=3 always-warm clamp theorem ===")
    cost_a = 0.08
    c_warm, c_switch_warm = 0.06, 0.01
    sol = nhop.always_warm_value_iteration_nhop(
        [hop1, hop2, hop3], cost_a, c_warm, c_switch_warm, resolution=20, n_iters=2000
    )
    print(f"g={sol.g:.6f}, grid points={sol.grid.n_points}")
    mono = nhop.check_monotone_nd(sol.d, sol.grid)
    total_viol = sum(v for k, v in mono.items() if k.startswith("n_violations"))
    print(f"monotonicity of d over 3 axes: {total_viol} total violations ({mono})")
    delta = sol.h[:, nhop.CONTEXT_B] - sol.h[:, nhop.CONTEXT_A]
    clamped = np.clip(sol.d, -c_switch_warm, c_switch_warm)
    print(f"clamp identity max error: {np.max(np.abs(delta - clamped)):.2e}")


if __name__ == "__main__":
    main()
