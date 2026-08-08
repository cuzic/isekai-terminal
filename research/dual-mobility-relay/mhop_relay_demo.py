"""task4-0 (task #59): sanity-check the new M=2-relay-arm exact grid solver
(`dmr/mhop_relay.py`) before using it for #51's falsification attempt.

Checks:
  1. Symmetric scenario (relay1 == relay2, same cost): the solved value
     function and policy should be exactly symmetric under (beta1,beta2) ->
     (beta2,beta1) swap combined with route R1<->R2 swap -- a basic
     correctness check independent of any monotonicity conjecture.
  2. Degenerate scenario (relay2 極端に悪い/always bad): routing should
     essentially never choose R2.
  3. Reports g (long-run average cost) and a few sample policy values for a
     visual sanity check.

Run with: uv run python mhop_relay_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, mhop_relay


def check_symmetry(relay: channels.HopParams, cost_a: float, c_switch: float, resolution: int = 40) -> None:
    sol = mhop_relay.mhop_relay_value_iteration(relay, relay, cost_a, c_switch, resolution=resolution, n_iters=1500)
    grid = sol.grid
    h_grid = sol.h.reshape(grid.shape[0], grid.shape[1], 3)
    # h[:, :, ROUTE_A] should be symmetric under beta1<->beta2 swap (transpose).
    h_a = h_grid[:, :, mhop_relay.ROUTE_A]
    max_asym_a = float(np.max(np.abs(h_a - h_a.T)))
    # h[:, :, ROUTE_R1] transposed should match h[:, :, ROUTE_R2] (context swap).
    h_r1 = h_grid[:, :, mhop_relay.ROUTE_R1]
    h_r2 = h_grid[:, :, mhop_relay.ROUTE_R2]
    max_asym_r = float(np.max(np.abs(h_r1 - h_r2.T)))
    print(f"Symmetric-scenario check: max |h_A - h_A^T| = {max_asym_a:.3e}, "
          f"max |h_R1 - h_R2^T| = {max_asym_r:.3e} (both should be ~0)")


def main() -> None:
    relay = channels.HopParams(p_gb=0.03, p_bg=0.3, eps_good=0.02, eps_bad=0.4)
    check_symmetry(relay, cost_a=0.1, c_switch=0.05)

    print("\n=== Degenerate scenario: relay2 almost always bad ===")
    relay1 = channels.HopParams(p_gb=0.02, p_bg=0.5, eps_good=0.01, eps_bad=0.15)
    relay2 = channels.HopParams(p_gb=0.5, p_bg=0.02, eps_good=0.01, eps_bad=0.9)  # mostly stuck Bad
    sol = mhop_relay.mhop_relay_value_iteration(relay1, relay2, cost_a=0.1, c_switch=0.05, resolution=60, n_iters=2000)
    frac_r2 = float(np.mean(sol.policy == mhop_relay.ROUTE_R2))
    print(f"g = {sol.g:.6f}, fraction of (belief,context) grid points where R2 is chosen: {frac_r2:.4f}")
    print("(should be near 0 -- R2 is almost never worth routing to)")

    print("\n=== Representative scenario, resolution=80 ===")
    relay1 = channels.HopParams(p_gb=0.05, p_bg=0.4, eps_good=0.02, eps_bad=0.3)
    relay2 = channels.HopParams(p_gb=0.03, p_bg=0.2, eps_good=0.01, eps_bad=0.5)
    sol = mhop_relay.mhop_relay_value_iteration(relay1, relay2, cost_a=0.12, c_switch=0.03, resolution=80, n_iters=2000)
    print(f"g = {sol.g:.6f}")
    for r, name in enumerate(["A", "R1", "R2"]):
        frac = float(np.mean(sol.policy[:, r] == r))
        print(f"  context={name}: fraction staying on {name} = {frac:.4f}")

    for ctx in range(3):
        mono = mhop_relay.stay_region_monotone_check(sol, ctx)
        print(f"  stay-region(context={['A', 'R1', 'R2'][ctx]}) multi-transition check: {mono}")


if __name__ == "__main__":
    main()
