"""Follow-up to R4 (task list items #69-72) and a Fable+Kimi consultation on
what to try next: does the homotopy witness's monotonicity-violation
location (beta1~0.87, beta2~0.09-0.10 -- resolution-stable 30 through 300,
see THRESHOLD_PROOF.md's 2026-07-19 "REFUTED" entry) coincide with where
the one-step value-of-information gap `J(beta) = h(predict(beta)) -
E_o[h(posterior(beta))]` (`invariant_features_demo.py`'s `voi_gap`, the
diagnosed MECHANISM behind Gap G1's non-monotonicity -- see
THRESHOLD_PROOF.md §4's exact 3-term Q-value decomposition) itself peaks?

Two possible outcomes:
  - MATCH: the d-field dip sits at (or very near) the VoI-gap's own peak
    along the same belief slice -- direct, mechanistic confirmation that
    THIS specific counterexample is a necessary consequence of the VoI-hump
    mechanism, not a coincidence or a different, unexplained phenomenon.
  - MISMATCH: the dip sits somewhere the VoI-gap is small/flat -- meaning
    either the VoI-gap by itself isn't the whole story for the POLICY-level
    (not just field-level) break, or a second mechanism is at play.

Run with: uv run python voi_gap_mechanism_check_demo.py
"""

from __future__ import annotations

import json

import numpy as np

from dmr import beliefgrid2d, channels
from invariant_features_demo import voi_gap
from policy_multicrossing_targeted_search_demo import solve_and_get_d
from policy_multicrossing_reachable_search_demo import reachable_box

# The homotopy witness: trial 23's params with hop2's p_bg shrunk to the
# value where the resolution-convergence study (THRESHOLD_PROOF.md) was
# actually done.
with open("output/adversarial_search_log.json") as f:
    _LOG = json.load(f)
_TRIAL23 = [t for t in _LOG["trials"] if t["trial"] == 23][0]["params"]
WITNESS_PARAMS = dict(_TRIAL23)
WITNESS_PARAMS["p_bg2"] = 0.00466

# The d-field dip's own resolution-stable location (context p=0, w=0; direction
# "beta2" means: fixed beta1 row, dip as beta2 varies) from the R4 continuation.
DIP_CONTEXT = (0, 0)
DIP_BETA1 = 0.873  # fixed row this dip sits on (stable across 100/200/300)
DIP_BETA2 = 0.093  # dip's own location along that row (stable across 100/200/300)


def main() -> None:
    hop1 = channels.HopParams(p_gb=WITNESS_PARAMS["p_gb1"], p_bg=WITNESS_PARAMS["p_bg1"],
                               eps_good=WITNESS_PARAMS["eps_good1"], eps_bad=WITNESS_PARAMS["eps_bad1"])
    hop2 = channels.HopParams(p_gb=WITNESS_PARAMS["p_gb2"], p_bg=WITNESS_PARAMS["p_bg2"],
                               eps_good=WITNESS_PARAMS["eps_good2"], eps_bad=WITNESS_PARAMS["eps_bad2"])
    lo1, hi1 = reachable_box(hop1.p_gb, hop1.p_bg)
    lo2, hi2 = reachable_box(hop2.p_gb, hop2.p_bg)
    print(f"witness hop1: p_gb={hop1.p_gb:.5f}, p_bg={hop1.p_bg:.5f}, lambda1={1-hop1.p_gb-hop1.p_bg:+.4f}, "
          f"reachable beta1 in [{lo1:.4f},{hi1:.4f}]")
    print(f"witness hop2: p_gb={hop2.p_gb:.5f}, p_bg={hop2.p_bg:.5f}, lambda2={1-hop2.p_gb-hop2.p_bg:+.4f}, "
          f"reachable beta2 in [{lo2:.4f},{hi2:.4f}]")
    print(f"hop2 fixed point beta* = p_gb/(p_gb+p_bg) = {hop2.p_gb/(hop2.p_gb+hop2.p_bg):.4f} "
          f"(Kimi's hypothesized -- and Fable-refuted -- coincidence target)")
    print(f"d-field dip location (from R4): context={DIP_CONTEXT}, beta1={DIP_BETA1}, beta2={DIP_BETA2}\n")

    for resolution in [100, 150]:
        sol = solve_and_get_d(WITNESS_PARAMS, resolution=resolution, n_iters=2500)
        p, w = DIP_CONTEXT
        gap = voi_gap(sol.grid, hop1, hop2, sol.h[:, p, w])
        gap_grid = gap.reshape(sol.grid.shape)
        axis = sol.grid.axis

        # Row nearest the dip's own beta1 location.
        row_idx = int(np.argmin(np.abs(axis - DIP_BETA1)))
        row = gap_grid[row_idx, :]
        peak_idx = int(np.argmax(row))
        peak_beta2 = axis[peak_idx]
        dip_col_idx = int(np.argmin(np.abs(axis - DIP_BETA2)))
        gap_at_dip = row[dip_col_idx]
        gap_at_peak = row[peak_idx]

        print(f"--- resolution={resolution} ---")
        print(f"  row beta1={axis[row_idx]:.4f} (nearest to dip's {DIP_BETA1}):")
        print(f"    VoI-gap J(beta) peaks at beta2={peak_beta2:.4f} (value={gap_at_peak:.4e})")
        print(f"    VoI-gap J(beta) AT the dip's own beta2={axis[dip_col_idx]:.4f}: value={gap_at_dip:.4e}")
        print(f"    distance from dip location to VoI-gap peak: {abs(peak_beta2 - axis[dip_col_idx]):.4f}")

        # Also report the GLOBAL peak of the VoI-gap across the whole grid (not just this row),
        # restricted to the reachable box, to see if it's somewhere else entirely.
        reach1 = (axis >= lo1) & (axis <= hi1)
        reach2 = (axis >= lo2) & (axis <= hi2)
        masked = gap_grid.copy()
        masked[~reach1, :] = -np.inf
        masked[:, ~reach2] = -np.inf
        gi, gj = np.unravel_index(np.argmax(masked), masked.shape)
        print(f"    global VoI-gap peak (within reachable box): beta1={axis[gi]:.4f}, beta2={axis[gj]:.4f}, "
              f"value={masked[gi,gj]:.4e}")
        print()

    print("=== Verdict ===")
    print("If the dip's own beta2 location is at or very near where J(beta) peaks along the same")
    print("row (small distance, ideally << the row's own characteristic width), that is direct")
    print("mechanistic evidence the VoI-hump term is what's producing this specific counterexample.")
    print("If the peak sits far from the dip, or the global VoI-gap peak is somewhere unrelated,")
    print("that's evidence the VoI-gap diagnostic alone doesn't explain this witness -- a second,")
    print("not-yet-identified factor (e.g., the continuation-value term, or the min-over-actions")
    print("interaction switching_curves.d_field_full_model's docstring already flags as a separate")
    print("risk point) would need to be examined instead.")


if __name__ == "__main__":
    main()
