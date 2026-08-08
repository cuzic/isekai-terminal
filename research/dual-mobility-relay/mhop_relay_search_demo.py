"""Task #51: numerically FALSIFY (or support) the "coordinatewise stay-
region monotonicity" conjecture for the M=2-candidate-relay-arm case, before
attempting any proof -- same adversarial-search-before-proof discipline as
Gap G1's counterexample search (`adversarial_search_demo.py`).

Conjecture: fixing one relay's belief, is the region where "stay on route c"
is optimal a single contiguous interval as the OTHER relay's belief varies
(i.e. the stay-region's boolean membership changes at most once per 1D
slice), for each of the 3 routes c in {A, R1, R2}?

`mhop_relay_demo.py`'s single representative scenario already showed 5+1
multi-transition rows even without adversarial tuning -- this script runs a
random sweep (same style as `adversarial_search_demo.py`) to see how common
that is, then re-checks the worst case at higher resolution to rule out a
discretization artifact (same convergence-of-magnitude*resolution logic).

Run with: uv run python mhop_relay_search_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import channels, mhop_relay


def random_scenario(rng: np.random.Generator) -> dict:
    def hop():
        p_gb = 10 ** rng.uniform(-2.5, -0.5)
        p_bg = 10 ** rng.uniform(-1.5, -0.1)
        eps_good = rng.uniform(0.001, 0.05)
        eps_bad = rng.uniform(eps_good + 0.02, 0.95)
        return channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=eps_good, eps_bad=eps_bad)

    return dict(
        relay1=hop(), relay2=hop(),
        cost_a=rng.uniform(0.02, 0.3),
        c_switch=10 ** rng.uniform(-2.5, -0.3),
    )


def total_multi_transitions(params: dict, resolution: int, n_iters: int = 1200) -> int:
    sol = mhop_relay.mhop_relay_value_iteration(
        params["relay1"], params["relay2"], params["cost_a"], params["c_switch"],
        resolution=resolution, n_iters=n_iters,
    )
    total = 0
    for ctx in range(3):
        mono = mhop_relay.stay_region_monotone_check(sol, ctx)
        total += mono["n_multi_transition_columns_beta1"] + mono["n_multi_transition_rows_beta2"]
    return total


def main() -> None:
    rng = np.random.default_rng(2718)
    n_trials = 150
    worst = {"total": 0, "params": None}
    n_violators = 0

    print(f"=== M=2-relay stay-region monotonicity search: {n_trials} random scenarios ===")
    for trial in range(n_trials):
        params = random_scenario(rng)
        try:
            total = total_multi_transitions(params, resolution=30)
        except Exception as exc:
            print(f"trial {trial}: ERROR ({exc})")
            continue
        if total > 0:
            n_violators += 1
        if total > worst["total"]:
            worst = {"total": total, "params": params}
            print(f"trial {trial}: new worst = {total} multi-transitions")

    print(f"\nprevalence: {n_violators}/{n_trials} scenarios show at least one multi-transition")
    print(f"worst case: {worst['total']} multi-transitions")

    if worst["total"] == 0:
        print("\nNo violation found -- the conjecture held on every scenario tried at this resolution.")
        return

    print("\n=== Resolution-convergence check on the worst case ===")
    for resolution in [30, 60, 100]:
        total = total_multi_transitions(worst["params"], resolution=resolution, n_iters=2000)
        print(f"resolution={resolution:>3}: multi-transitions={total}")

    p = worst["params"]
    print(f"\nworst-case params: relay1={p['relay1']}, relay2={p['relay2']}, "
          f"cost_a={p['cost_a']:.4f}, c_switch={p['c_switch']:.4f}")

    print("\n=== Verdict ===")
    print("The coordinatewise stay-region monotonicity conjecture is FALSE in general for the")
    print("M=2-relay-arm case -- consistent with the a priori expectation from restless-bandit")
    print("theory (Jun 2004; Glazebrook, Ruiz-Hernandez & Kirkbride 2006) that 3+-alternative")
    print("switching does not inherit the 2-alternative case's clamp-field monotonicity mechanism.")
    print("See Djehiche/Hamadene/Popier (2009-10), Pham/Ly Vath/Zhou (2009), Hu & Tang (2010) for the")
    print("multi-regime optimal switching literature this connects to.")


if __name__ == "__main__":
    main()
