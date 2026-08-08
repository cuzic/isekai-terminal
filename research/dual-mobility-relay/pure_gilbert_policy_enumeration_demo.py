"""Step 2 of the pure-Gilbert analytical program (per Fable-model guidance, 2026-07-19):
enumerate the DISTINCT optimal policies that actually appear across a broad parameter sweep of
the validated 8-state finite MDP (`pure_gilbert_finite_mdp_demo.py`), before attempting to solve
any of them symbolically. Only a handful of the 2^8=256 theoretically possible policies are
expected to actually be optimal anywhere in the physically-plausible parameter range -- solving
only those (not all 256) by computer algebra is the tractable path to an explicit g_warm formula.

Run with: uv run python pure_gilbert_policy_enumeration_demo.py
"""

from __future__ import annotations

import itertools

import numpy as np

from dmr import channels
from pure_gilbert_finite_mdp_demo import build_finite_mdp, solve_average_cost_policy_iteration, STATE_LABELS


def policy_signature(policy: np.ndarray) -> tuple:
    return tuple(int(x) for x in policy)


def describe_policy(policy: np.ndarray) -> str:
    parts = []
    for s in range(8):
        c, p = s // 2, s % 2
        parts.append(f"{STATE_LABELS[c]}|{'A' if p==0 else 'B'}->{'A' if policy[s]==0 else 'B'}")
    return " ".join(parts)


def main() -> None:
    rng = np.random.default_rng(0)
    seen = {}

    # Sweep: independent p_gb1,p_bg1,p_gb2,p_bg2 (asymmetric), cost_a, c_warm, c_switch_warm.
    n_trials = 400
    for _ in range(n_trials):
        p_gb1 = rng.uniform(0.01, 0.5)
        p_bg1 = rng.uniform(0.01, 0.5)
        p_gb2 = rng.uniform(0.01, 0.5)
        p_bg2 = rng.uniform(0.01, 0.5)
        cost_a = rng.uniform(0.05, 0.5)
        c_warm = rng.uniform(0.001, 0.1)
        c_switch_warm = rng.uniform(0.001, 0.05)

        hop1 = channels.HopParams(p_gb=p_gb1, p_bg=p_bg1, eps_good=0.0, eps_bad=1.0)
        hop2 = channels.HopParams(p_gb=p_gb2, p_bg=p_bg2, eps_good=0.0, eps_bad=1.0)

        T, cost = build_finite_mdp(hop1, hop2, cost_a, c_warm, c_switch_warm)
        g, h, policy, n_iter = solve_average_cost_policy_iteration(T, cost)
        sig = policy_signature(policy)
        if sig not in seen:
            seen[sig] = dict(count=0, example=(p_gb1, p_bg1, p_gb2, p_bg2, cost_a, c_warm, c_switch_warm), policy=policy)
        seen[sig]["count"] += 1

    print(f"=== {len(seen)} distinct optimal policies found across {n_trials} random trials ===\n")
    for sig, info in sorted(seen.items(), key=lambda kv: -kv[1]["count"]):
        print(f"count={info['count']:4d}  {describe_policy(info['policy'])}")
        p_gb1, p_bg1, p_gb2, p_bg2, cost_a, c_warm, c_switch_warm = info["example"]
        print(f"           example params: p_gb1={p_gb1:.3f} p_bg1={p_bg1:.3f} p_gb2={p_gb2:.3f} p_bg2={p_bg2:.3f} "
              f"cost_a={cost_a:.3f} c_warm={c_warm:.3f} c_switch_warm={c_switch_warm:.3f}")
        print()


if __name__ == "__main__":
    main()
