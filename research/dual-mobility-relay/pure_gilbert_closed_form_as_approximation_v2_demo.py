"""Second attempt at the distillation/approximation-error question (see
pure_gilbert_closed_form_as_approximation_demo.py for the first, mis-scaled attempt): use this
project's ACTUAL calibrated operating point (c_warm=0.005, c_switch_warm=0.01, c_switch_cold=0.02,
from TRACE_CALIBRATION_NOTES.md's peak-gain point) and the REAL Berlin V2X hops' own (pi_b,lambda)
values, rather than an arbitrary synthetic point -- so the eps-degradation test is directly
comparable to the real-data case in THRESHOLD_PROOF.md §6.

For each real hop (used symmetrically, hop1=hop2=that hop's own p_gb/p_bg), sweeps eps_good/
eps_bad from the pure-Gilbert idealization (0,1) toward that hop's OWN real calibrated eps values,
and measures how far the true crossing (found via the general switching_curves solver) departs
from the pure-Gilbert closed form's fixed prediction (which ignores eps entirely).

Run with: uv run python pure_gilbert_closed_form_as_approximation_v2_demo.py
"""

from __future__ import annotations

from dmr import channels, switching_curves

C_WARM = 0.005
C_SWITCH_WARM = 0.01
C_SWITCH_COLD = 0.02
RESOLUTION = 60
N_ITERS = 2000

HOPS = {
    "hop1-like (pi_b=0.2954, lambda=0.354)": dict(p_gb=0.1909, p_bg=0.4553, real_eps=(0.0320, 0.3010)),
    "hop2-like (pi_b=0.4127, lambda=0.330)": dict(p_gb=0.2764, p_bg=0.3933, real_eps=(0.0695, 0.4253)),
}


def cost_a_star_closed_form(p_gb: float, p_bg: float) -> float:
    lam = 1 - p_gb - p_bg
    pi_b = p_gb / (p_gb + p_bg)
    q_g = 1 - p_gb
    return C_WARM / (1 - pi_b) ** 2 + (1 - q_g ** 2) * (1 + 2 * C_SWITCH_WARM)


def phi(cost_a: float, p_gb: float, p_bg: float, eps_good: float, eps_bad: float) -> float:
    hop = channels.HopParams(p_gb=p_gb, p_bg=p_bg, eps_good=eps_good, eps_bad=eps_bad)
    sol_warm = switching_curves.always_warm_value_iteration(
        hop, hop, cost_a, C_WARM, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(
        hop, hop, cost_a, C_SWITCH_COLD, resolution=RESOLUTION, n_iters=N_ITERS)
    return sol_warm.g - sol_cold.g


def bisect_cost_a_star(p_gb: float, p_bg: float, eps_good: float, eps_bad: float,
                        lo: float = 0.05, hi: float = 0.6) -> float:
    f_lo = phi(lo, p_gb, p_bg, eps_good, eps_bad)
    f_hi = phi(hi, p_gb, p_bg, eps_good, eps_bad)
    tries = 0
    while f_lo * f_hi > 0 and hi < 0.95 and tries < 10:
        hi += 0.05
        f_hi = phi(hi, p_gb, p_bg, eps_good, eps_bad)
        tries += 1
    if f_lo * f_hi > 0:
        return float("nan")
    for _ in range(35):
        mid = (lo + hi) / 2
        f_mid = phi(mid, p_gb, p_bg, eps_good, eps_bad)
        if abs(f_mid) < 1e-5 or (hi - lo) < 1e-5:
            return mid
        if (f_mid > 0) == (f_lo > 0):
            lo, f_lo = mid, f_mid
        else:
            hi = mid
    return (lo + hi) / 2


def main() -> None:
    for label, hop_info in HOPS.items():
        p_gb, p_bg = hop_info["p_gb"], hop_info["p_bg"]
        real_eps_good, real_eps_bad = hop_info["real_eps"]
        approx = cost_a_star_closed_form(p_gb, p_bg)

        print(f"=== {label} ===")
        print(f"c_warm={C_WARM}, c_switch_warm={C_SWITCH_WARM}, c_switch_cold={C_SWITCH_COLD}")
        print(f"Pure-Gilbert closed-form cost_a*_approx = {approx:.5f} (eps-independent)\n")

        print(f"{'eps_good':>9} {'eps_bad':>9} {'cost_a*_true':>13} {'rel.err%':>9}  note")
        # interpolate from pure-Gilbert (0,1) toward this hop's own real (eps_good,eps_bad)
        steps = [0.0, 0.25, 0.5, 0.75, 1.0]
        for t in steps:
            eg = 0.0 + t * real_eps_good
            eb = 1.0 + t * (real_eps_bad - 1.0)
            true_val = bisect_cost_a_star(p_gb, p_bg, eg, eb)
            note = "pure-Gilbert (exact)" if t == 0.0 else ("REAL calibrated eps" if t == 1.0 else "")
            if true_val != true_val:  # nan check
                print(f"{eg:>9.4f} {eb:>9.4f} {'no crossing':>13} {'--':>9}  {note}")
            else:
                rel_err = abs(true_val - approx) / true_val * 100
                print(f"{eg:>9.4f} {eb:>9.4f} {true_val:>13.5f} {rel_err:>8.2f}%  {note}")
        print()


if __name__ == "__main__":
    main()
