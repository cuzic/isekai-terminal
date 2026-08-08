"""Replaces the two static (non-mobile) real datasets tried so far (`due/packet-delivery`,
`isti/rural`) with Berlin V2X -- real vehicular sidelink measurements from actual drives
through Berlin (Fraunhoferhhi/BerlinV2X, IEEE DataPort open access) -- specifically because
TRACE_CALIBRATION_NOTES.md's "SECOND CORRECTION" section found that BOTH static links only
showed a single one-time startup/shutdown transient, not the REPEATED persistence the routing
MDP actually needs, and diagnosed this as a structural consequence of testing non-mobile
links. Berlin V2X has genuine ongoing mobility (car-to-car sidelink during real drives), so a
persistence finding here would not share that specific failure mode.

Data preparation (done once, outside this script, via a `uv run --with pandas --with pyarrow`
one-off session -- NOT added to this project's own numpy/scipy/matplotlib pyproject.toml
dependencies, matching the project's established policy and its precedent for one-off external
tools like the earlier `.rar` extraction via `node-unrar-js`):
  1. Downloaded `sidelink_dataframe.parquet` from IEEE DataPort (DOI referenced in the dataset's
     `open-access/berlin-v2x` page) via the user's authenticated session.
  2. The parquet already has real per-1-second `Packet_error_ratio` (PER) and `Received Packets`
     counts per (Source car, Destination car, Scenario) sidelink pair; `Packet_transmission_rate_hz`
     is a fixed 50Hz for the pairs used here, confirming `total_sent = Received/(1-PER)` recovers
     an exact integer (50) every row -- so `n_trials=50, k_loss=total_sent-Received` per second is
     an EXACT reconstruction, not an approximation.
  3. Each (Source,Destination,Scenario) group's rows span MULTIPLE separate drive-rounds (real
     gaps of tens of minutes to hours between rounds, not one continuous recording) -- split into
     contiguous segments wherever the gap between consecutive `time_epoch` values exceeds 3s, and
     picked the single LONGEST contiguous segment per candidate pair, rather than naively binning
     across a session boundary (which would treat "car reconnected 20 minutes later" as if it were
     3 more seconds of continuous channel state).
  4. Saved as `data/berlin_v2x/berlin_v2x_seg{71,98}.npz` (per-second n_trials/k_loss arrays,
     not committed to git, same convention as the other real datasets in this project).

Two real, genuinely different car-to-car links, both much longer than either static dataset's
best segment: seg71 (car4->car2, scenario S2) 1873s continuous, 11.1% loss; seg98 (car3->car1,
scenario S2) 1105s continuous, 21.7% loss.

Run with: uv run python berlin_v2x_block_fit_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, ge_fit, switching_curves, warm_standby

HOP1_FILE = "data/berlin_v2x/berlin_v2x_seg71.npz"  # car4->car2, S2, 1873s, 11.1% loss
HOP2_FILE = "data/berlin_v2x/berlin_v2x_seg98.npz"  # car3->car1, S2, 1105s, 21.7% loss
DT_NATIVE_S = 1.0  # already 1-second-resolution PER data

T_EPOCH_S = 3.0  # rust-core isekai-transport/src/path_health.rs:52
COST_A = 0.30  # CORRECTED 2026-07-19: reusing the paper's cost_a=0.16 verbatim (as for the
# WSN real pair earlier in this project) made this Berlin V2X pair's combined path-B stationary
# loss (~30.4%, computed from the joint independent stationary distribution over both hops'
# fitted p_gb/p_bg -- see the ad hoc check that caught this) far worse than cost_a on average,
# degenerating EVERY policy to "always route A" (confirmed via debug print of the raw solved
# values: g_adapt=g_cold=0.16 exactly, g_warm=cost_a+c_warm exactly, a real but uninformative
# result with a gain sweep flat at 0.00% everywhere -- same failure mode this project already
# documented once for the WSN real pair). Raised to 0.30 (comparable to this real pair's own
# stationary path-B loss) for the same reason as before: give the relay a genuine chance to be
# worth using sometimes, rather than being arbitrarily dominated by path A on average.
C_SWITCH_WARM = 0.01
RESOLUTION = 60
N_ITERS = 2000


def load_per_second_arrays(path: str) -> tuple[np.ndarray, np.ndarray]:
    """`k_loss = total_sent - Received` where `total_sent = Received/(1-PER)` is an exact
    integer reconstruction for the vast majority of rows (confirmed during data prep: always
    resolves to exactly 50, the fixed transmission rate). But the dataset's own
    `Packet_error_ratio` column is itself rounded (2 decimal places), so occasionally the
    back-calculation is off by 1-2 packets -- confirmed: up to 56/1873 rows in seg71 and
    22/1105 in seg98 come out at `k_loss` in {-1,-2} instead of a valid non-negative count.
    This is a real, bounded data-quality artifact of the reconstruction (not a bug in this
    project's code), clipped here to the only physically valid range -- otherwise these
    negative counts corrupt the Binomial log-likelihood (`gammaln`/`log` of a negative/out-of-
    range argument), which is what caused an `invalid value encountered in subtract` warning
    inside `fit_ge_binomial_em` before this fix.
    """
    data = np.load(path)
    n_trials, k_loss = data["n_trials"], data["k_loss"]
    return n_trials, np.clip(k_loss, 0, n_trials)


def bin_prebinned_to_windows(n_trials_1s: np.ndarray, k_loss_1s: np.ndarray, t_window_s: float) -> tuple[np.ndarray, np.ndarray]:
    """Same idea as `ge_fit.bin_to_windows`, but for data that's already aggregated to 1-second
    granularity (n_trials/k_loss per second) rather than a raw per-packet binary sequence --
    just sums consecutive 1-second bins into `t_window_s`-second windows.
    """
    seconds_per_window = int(round(t_window_s / DT_NATIVE_S))
    n_full = len(n_trials_1s) // seconds_per_window
    n_trimmed = n_trials_1s[: n_full * seconds_per_window].reshape(n_full, seconds_per_window)
    k_trimmed = k_loss_1s[: n_full * seconds_per_window].reshape(n_full, seconds_per_window)
    return n_trimmed.sum(axis=1), k_trimmed.sum(axis=1)


def raw_autocorrelation_check(rate: np.ndarray, label: str) -> None:
    """Model-free sanity check, run BEFORE trusting any EM output -- per the lesson from
    isti_rural_block_fit_demo.py, where the EM's high-lambda answer turned out to be driven by
    a single one-time transient, not repeated bursting, and this exact kind of direct
    inspection of the raw per-window sequence (not the EM's internal machinery) is what caught it.
    """
    x = rate - rate.mean()
    n = len(x)

    def autocov(k: int) -> float:
        return float(np.mean(x[: n - k] * x[k:]))

    c0 = autocov(0)
    print(f"  [{label}] per-window loss rate: mean={rate.mean():.3f}, std={rate.std():.3f}, "
          f"min={rate.min():.3f}, max={rate.max():.3f}")
    print(f"  [{label}] autocorr: lag1={autocov(1)/c0:+.3f}, lag2={autocov(2)/c0:+.3f}, "
          f"lag3={autocov(3)/c0:+.3f}, lag5={autocov(5)/c0:+.3f}, lag10={autocov(10)/c0:+.3f}, "
          f"lag20={autocov(20)/c0:+.3f} (1/sqrt(n) white-noise reference={1/np.sqrt(n):.3f})")
    # crude single-transient-vs-repeated check: count how many times the smoothed rate crosses
    # its own median -- a single one-off transient crosses ~0-2 times; genuine repeated bursting
    # crosses many times.
    smoothed = np.convolve(rate, np.ones(5) / 5, mode="valid")
    median = np.median(smoothed)
    crossings = np.sum(np.diff((smoothed > median).astype(int)) != 0)
    print(f"  [{label}] median-crossings of a 5-window smoothed rate: {crossings} "
          f"(a handful of crossings => one-off transient; many => genuine repeated switching)")


def block_fit(path: str, label: str) -> tuple[channels.HopParams, float, np.ndarray]:
    n_1s, k_1s = load_per_second_arrays(path)
    n_trials, k_loss = bin_prebinned_to_windows(n_1s, k_1s, t_window_s=T_EPOCH_S)
    rate = k_loss / n_trials

    print(f"=== {label} ({path}) ===")
    print(f"  {len(n_1s)}s continuous recording, {len(n_trials)} windows @ {T_EPOCH_S}s, "
          f"overall loss rate={k_1s.sum()/n_1s.sum():.3f}")
    raw_autocorrelation_check(rate, label)

    best, all_starts = ge_fit.fit_ge_binomial_em_multistart(n_trials, k_loss, n_starts=30)
    lam = 1 - best.p_gb - best.p_bg
    logliks = sorted({round(ll, 2) for ll, _ in all_starts}, reverse=True)
    print(f"  BEST EM fit (of 30 starts, loglik={logliks[0]:.2f}): p_gb={best.p_gb:.4f}, "
          f"p_bg={best.p_bg:.4f}, eps_good={best.eps_good:.4f}, eps_bad={best.eps_bad:.4f}, "
          f"lambda={lam:+.3f}")
    if len(logliks) > 1:
        print(f"  ({len(logliks)} distinct local optima found across 30 starts; "
              f"next-best loglik={logliks[1]:.2f})")

    hop = channels.HopParams(p_gb=best.p_gb, p_bg=best.p_bg, eps_good=best.eps_good, eps_bad=best.eps_bad)
    return hop, lam, rate


def gain(hop1: channels.HopParams, hop2: channels.HopParams, c_switch_cold: float, c_warm: float = 0.02) -> float:
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, c_warm, C_SWITCH_WARM, c_switch_cold)
    sol_adapt = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_warm = switching_curves.always_warm_value_iteration(hop1, hop2, COST_A, c_warm, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(hop1, hop2, COST_A, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    baseline = min(sol_warm.g, sol_cold.g)
    return (baseline - sol_adapt.g) / baseline if baseline > 0 else 0.0


def main() -> None:
    hop1, lam1, rate1 = block_fit(HOP1_FILE, "hop1 (car4->car2)")
    print()
    hop2, lam2, rate2 = block_fit(HOP2_FILE, "hop2 (car3->car1)")

    print("\n=== Real-pair gain sweep (BOTH hops from real, MOBILE Berlin V2X segments) ===")
    c_switch_values = [0.017, 0.033, 0.067, 0.1, 0.167, 0.333, 0.667, 1.333, 2.67]
    print(f"{'c_switch':>10} {'gain':>10}")
    gains = []
    for cs in c_switch_values:
        g = gain(hop1, hop2, cs)
        gains.append(g)
        print(f"{cs:>10.3f} {g * 100:>9.2f}%")

    print("\n=== Verdict ===")
    print(f"hop1 lambda={lam1:+.3f}, hop2 lambda={lam2:+.3f} (real vehicular mobility, "
          f"{len(rate1)} and {len(rate2)} windows respectively).")
    max_gain = max(gains)
    above_5pct = [c for c, g in zip(c_switch_values, gains) if g > 0.05]
    if max_gain > 0.05:
        print(f"Peak gain {max_gain*100:.1f}% -- clears the 5% bar for c_switch in "
              f"{above_5pct if above_5pct else '(none)'}.")
    else:
        print(f"Peak gain {max_gain*100:.1f}% -- does NOT clear the 5% bar anywhere in the swept range.")


if __name__ == "__main__":
    main()
