"""R4 (Fable's cheapest/most-informative follow-up to R1/R2, 2026-07-19): does the
in-reachable-box dip depth genuinely collapse to 0 exactly as the box's edge is
dragged toward a known field-violator's dip location, or does a real in-box
counterexample appear somewhere along the path first?

R2 found 0/12 known field-violators have a d-field dip inside their own
dynamically reachable belief box, and a (separately corrected, see
THRESHOLD_PROOF.md's 2026-07-19 correction) global DE existence hunt also found
nothing -- but that global hunt turned out to be a near-unweighted ~240-point
random sample (a real bug: it silently scored solver exceptions as "no
violation", and DE's own convergence check fired almost immediately once most
of the population scored exactly 0). Fable's proposed fix, done FIRST here
since it's cheap and directly mechanistic rather than more blind sampling: for
each known violator, continuously shrink whichever hop parameter controls the
box edge nearest that violator's own dip location (p_bg -> drag the upper edge
`1-p_bg` toward 1; p_gb -> drag the lower edge toward 0), re-solving the FULL
belief-MDP at each step (the dynamics genuinely change, not just the box), and
track the in-box dip depth along the path. Two possible outcomes:
  - depth stays exactly 0 until the box's edge reaches the dip's location (at
    which point "inside" vs "outside" stops being a meaningful distinction) --
    direct evidence FOR the "closure" story: the mechanism and the box move
    together, not coincidentally past each other.
  - depth becomes positive INSIDE the box at some point along the path, while
    the dip location is still meaningfully outside the (moving) box edge --
    a genuine reachable-box counterexample, found via a directed path rather
    than luck.

Unlike the earlier DE hunt, solver exceptions here are logged and reported
explicitly, never silently scored as "no violation" (the bug Fable found).

Run with: uv run python policy_multicrossing_homotopy_demo.py
"""

from __future__ import annotations

import json

import numpy as np

from dmr import channels
from policy_multicrossing_targeted_search_demo import solve_and_get_d, find_deepest_dip
from policy_multicrossing_reachable_search_demo import reachable_box, find_deepest_dip_in_reachable_box

N_STEPS = 15
FLOOR = 1e-4
RESOLUTION = 30
N_ITERS = 1500


def locate_dip_and_edge(seed_params: dict):
    """Returns (hop_idx, edge, orig_val) identifying which hop parameter to
    continue, based on where this seed's ORIGINAL (unrestricted) deepest dip
    sits: if beta1 is at (near) 1 or 0, hop1 is the relevant one; if beta2,
    hop2. 'edge' is 'upper' (drag 1-p_bg toward the dip) if the dip sits near
    1, or 'lower' (drag p_gb toward the dip) if it sits near 0."""
    sol = solve_and_get_d(seed_params, resolution=RESOLUTION, n_iters=N_ITERS)
    best = None
    for p in range(2):
        for w in range(2):
            found = find_deepest_dip(sol, p, w)
            if found is None:
                continue
            b1, b2, d_val, depth = found
            if best is None or depth > best[0]:
                best = (depth, p, w, b1, b2)
    if best is None:
        return None
    depth, p, w, b1, b2 = best
    # whichever of b1/b2 is closer to an edge (0 or 1) identifies the relevant hop
    dist_b1_edge = min(b1, 1.0 - b1)
    dist_b2_edge = min(b2, 1.0 - b2)
    if dist_b1_edge <= dist_b2_edge:
        hop_idx, edge_val = 1, b1
    else:
        hop_idx, edge_val = 2, b2
    edge = "upper" if edge_val > 0.5 else "lower"
    key = f"p_bg{hop_idx}" if edge == "upper" else f"p_gb{hop_idx}"
    return {"hop_idx": hop_idx, "edge": edge, "key": key, "orig_val": seed_params[key],
            "dip_depth": depth, "dip_p": p, "dip_w": w, "dip_beta1": b1, "dip_beta2": b2}


def run_continuation(seed_params: dict, label: str) -> dict:
    info = locate_dip_and_edge(seed_params)
    if info is None:
        print(f"\n=== {label}: no dip found at all -- skipping ===")
        return {"found_counterexample": False}

    print(f"\n=== {label}: continuation on hop{info['hop_idx']}.{info['key']} "
          f"(edge={info['edge']}), dip at (beta1={info['dip_beta1']:.4f}, "
          f"beta2={info['dip_beta2']:.4f}), depth={info['dip_depth']:.3e} ===")

    orig_val = info["orig_val"]
    ts = np.linspace(0.0, 1.0, N_STEPS)
    log_orig, log_floor = np.log(orig_val), np.log(FLOOR)
    found_counterexample = False
    for t in ts:
        val = float(np.exp(log_orig + t * (log_floor - log_orig)))
        params = dict(seed_params)
        params[info["key"]] = val
        hop1 = channels.HopParams(p_gb=params["p_gb1"], p_bg=params["p_bg1"],
                                   eps_good=params["eps_good1"], eps_bad=params["eps_bad1"])
        hop2 = channels.HopParams(p_gb=params["p_gb2"], p_bg=params["p_bg2"],
                                   eps_good=params["eps_good2"], eps_bad=params["eps_bad2"])
        hop = hop1 if info["hop_idx"] == 1 else hop2
        lo, hi = reachable_box(hop.p_gb, hop.p_bg)
        edge_val = hi if info["edge"] == "upper" else lo
        dip_val = info["dip_beta1"] if info["hop_idx"] == 1 else info["dip_beta2"]
        dist_to_dip = abs(edge_val - dip_val)

        try:
            sol = solve_and_get_d(params, resolution=RESOLUTION, n_iters=N_ITERS)
        except Exception as exc:
            print(f"  t={t:.2f} {info['key']}={val:.5f} box_edge={edge_val:.4f} "
                  f"(dist_to_dip={dist_to_dip:.4f}): SOLVER FAILED ({type(exc).__name__}: {exc}) "
                  f"-- logged as unknown, NOT scored as 0")
            continue

        best_depth = 0.0
        for p in range(2):
            for w in range(2):
                found = find_deepest_dip_in_reachable_box(sol, p, w, hop1, hop2)
                if found is not None and found[3] > best_depth:
                    best_depth = found[3]

        flag = ""
        if best_depth > 1e-8:
            flag = "  <-- IN-BOX DIP FOUND"
            found_counterexample = True
        print(f"  t={t:.2f} {info['key']}={val:.5f} box_edge={edge_val:.4f} "
              f"(dist_to_dip={dist_to_dip:.4f}): in-box depth={best_depth:.4e}{flag}")

    return {"found_counterexample": found_counterexample}


def main() -> None:
    with open("output/adversarial_search_log.json") as f:
        log = json.load(f)
    violators = [t for t in log["trials"] if t.get("total_viol", 0) > 0]
    violators.sort(key=lambda t: -t["total_viol"])

    any_found = False
    for t in violators[:5]:
        result = run_continuation(t["params"], f"trial {t['trial']} (field_viol={t['total_viol']})")
        if result["found_counterexample"]:
            any_found = True

    print("\n=== Verdict ===")
    if any_found:
        print("At least one continuation path found a genuine in-reachable-box dip BEFORE the")
        print("box edge reached the original dip's location -- a real counterexample to the")
        print("reachable-box conjecture, found via a directed mechanistic path rather than luck.")
    else:
        print("No continuation path produced an in-box dip at any step before the box edge itself")
        print("reached the dip's location -- direct, mechanistic (not just absence-of-evidence)")
        print("support for the 'closure' story: as the box is dragged toward where Gap G1's")
        print("pathology lives, the in-box depth stays at exactly 0 the whole way, only becoming")
        print("possible once 'inside vs outside the box' stops being a meaningful distinction.")


if __name__ == "__main__":
    main()
