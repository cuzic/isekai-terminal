"""R3 (task #67's gating check, per Fable's 2026-07-19 review): before any
M>=3 extension, MHOP_RELAY_NOTES.md itself flags that the M=2 stay-region
disconnection finding (genuine flicker in 9/150 scenarios, task #68's
corrected count) has never been put through a resolution-convergence check
comparable to the one that validated Gap G1 as real (not a discretization
artifact) in `localize_violations_demo.py`/`adversarial_search_demo.py`
(`violation-magnitude * resolution` or envelope-deficit-AREA converging to
a nonzero constant across resolutions).

This script builds the missing diagnostic for the M=2 case: for each
disconnected 1D slice (a row/column where the stay-region has >1 connected
component, per `stay_region_connected_components_check`), measure the total
CONTINUOUS-coordinate width of the internal gap(s) between components (not
just a raw cell/index count, which trivially grows with resolution even for
a single-cell artifact). If this width converges to a fixed positive value
as resolution increases, the disconnection is a real geometric feature of
the continuous stay-region; if it shrinks like ~1/resolution toward 0, it is
a discretization fluke and the 6% prevalence figure should not be trusted as
describing a real phenomenon.

Uses the same corrected worst-case M=2 witness as
`mhop_relay_geometry_demo.py` (task #66).

Run with: uv run python mhop_relay_gap_convergence_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, mhop_relay

RELAY1 = channels.HopParams(p_gb=0.018846684946415816, p_bg=0.1107606703446672,
                             eps_good=0.013130216214149122, eps_bad=0.44148946798907146)
RELAY2 = channels.HopParams(p_gb=0.0366049350393019, p_bg=0.03589818673588046,
                             eps_good=0.034764642885318026, eps_bad=0.5317276749980895)
COST_A, C_SWITCH = 0.2899, 0.2367

CONTEXT_NAMES = ["A", "R1", "R2"]


def internal_gap_width(stay_row: np.ndarray, dx: float) -> tuple[float, int]:
    """Total continuous-coordinate width of False-run(s) strictly between
    the first and last True cell on this 1D slice (0.0 if the slice has <=1
    connected True-component, i.e. no internal gap). Also returns the raw
    cell count (which trivially grows with resolution and is NOT by itself
    evidence of a real feature -- included only for comparison)."""
    idx_true = np.where(stay_row)[0]
    if len(idx_true) < 2:
        return 0.0, 0
    first, last = idx_true[0], idx_true[-1]
    span = stay_row[first:last + 1]
    n_false_internal = int(np.count_nonzero(~span))
    return n_false_internal * dx, n_false_internal


def worst_and_mean_gap(solution, context: int, dx: float) -> dict:
    stay = (solution.policy[:, context] == context).astype(int).reshape(solution.grid.shape)
    col_widths = [internal_gap_width(stay[:, j], dx)[0] for j in range(stay.shape[1])]  # beta1-direction
    row_widths = [internal_gap_width(stay[i, :], dx)[0] for i in range(stay.shape[0])]  # beta2-direction
    all_widths = col_widths + row_widths
    n_disconnected = sum(1 for w in all_widths if w > 0)
    return {
        "max_gap_width": max(all_widths, default=0.0),
        "mean_gap_width_over_disconnected": (float(np.mean([w for w in all_widths if w > 0]))
                                              if n_disconnected else 0.0),
        "n_disconnected_slices": n_disconnected,
        "n_slices_total": len(all_widths),
    }


def main() -> None:
    print("=== R3: does the M=2 stay-region disconnection converge to a real (nonzero) gap width, ===")
    print("=== or shrink toward 0 as a discretization artifact, as resolution increases? ===\n")

    resolutions = [30, 60, 100, 150, 200, 300]
    results = {ctx: [] for ctx in range(3)}
    for resolution in resolutions:
        sol = mhop_relay.mhop_relay_value_iteration(RELAY1, RELAY2, COST_A, C_SWITCH,
                                                      resolution=resolution, n_iters=2500)
        dx = sol.grid.axis[1] - sol.grid.axis[0]
        print(f"--- resolution={resolution} (dx={dx:.5f}) ---")
        for ctx in range(3):
            r = worst_and_mean_gap(sol, ctx, dx)
            results[ctx].append((resolution, r))
            if r["n_disconnected_slices"] > 0:
                print(f"  context={CONTEXT_NAMES[ctx]}: {r['n_disconnected_slices']}/{r['n_slices_total']} "
                      f"disconnected slices, max gap width={r['max_gap_width']:.5f}, "
                      f"mean gap width={r['mean_gap_width_over_disconnected']:.5f}")
            else:
                print(f"  context={CONTEXT_NAMES[ctx]}: no disconnection at this resolution")

    print("\n=== Convergence summary per context (max gap width across resolutions) ===")
    any_real = False
    for ctx in range(3):
        widths = [r["max_gap_width"] for _, r in results[ctx]]
        print(f"context={CONTEXT_NAMES[ctx]}: max_gap_width per resolution = "
              f"{[f'{w:.5f}' for w in widths]}")
        nonzero = [w for w in widths if w > 0]
        if len(nonzero) >= 3:
            # crude convergence signal: does the width stay within a factor of 2 across the
            # last 3 (finest) resolutions tested, rather than roughly halving each time
            # (which is what a fixed-cell-count artifact would do)?
            last3 = nonzero[-3:]
            ratio = max(last3) / min(last3) if min(last3) > 0 else float("inf")
            print(f"  last 3 nonzero widths: {[f'{w:.5f}' for w in last3]}, max/min ratio={ratio:.2f}")
            if ratio < 2.0:
                any_real = True
                print(f"  -> STABLE (ratio<2x): consistent with a real geometric gap, not an artifact")
            else:
                print(f"  -> SHRINKING/UNSTABLE (ratio>=2x): consistent with a discretization artifact")

    print("\n=== Verdict ===")
    if any_real:
        print("At least one context's disconnection gap width is STABLE across resolutions 30-300 --")
        print("this is real evidence the M=2 stay-region disconnection is a genuine geometric feature")
        print("of the continuous model, not a discretization fluke. The 6% prevalence figure")
        print("(task #68) can be trusted as describing something real. An M=3 extension investigating")
        print("the underlying mechanism would now be gated-in, not gated-out.")
    else:
        print("No context showed a resolution-stable gap width -- the M=2 disconnection this witness")
        print("exhibits shrinks toward zero as resolution increases, consistent with a discretization")
        print("artifact rather than a genuine geometric feature of the continuous stay-region. This")
        print("means the 6% prevalence figure (task #68) should NOT be trusted as describing a real")
        print("phenomenon without re-checking on a witness that DOES show convergence, and no M>=3")
        print("extension should be attempted until a genuinely convergent instance is found.")


if __name__ == "__main__":
    main()
