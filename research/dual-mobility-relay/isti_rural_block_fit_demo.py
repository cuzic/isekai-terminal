"""Replaces `due/packet-delivery` with CRAWDAD `isti/rural` (IEEE DataPort, DOI
10.15783/C7G01C) as the real-data source for the 3s-block GE persistence question left
unresolved in TRACE_CALIBRATION_NOTES.md's "RESOLUTION" section: `due/packet-delivery`'s
fixed 3601-packet-per-config recordings gave only 12-36 windows at a 3s epoch, which a
synthetic ground-truth stress test showed is statistically uninformative (n=12: lambda
estimates ranging -0.29 to +0.90 when true=0.80).

`isti/rural` fixes exactly this: 200,000 frames per trace at a constant 5ms (11/5.5/2Mb/s)
or 10ms (1Mb/s) inter-transmission time -- outdoor 802.11b ad hoc, Navacchio (Pisa) rural
field trial, April 2006 -- giving ~1000s (16.7min) per trace, ~330 windows at a 3s epoch
(vs. due/packet-delivery's 12-36). Real per-frame binary loss status, not aggregated.

File format (reverse-engineered from the raw data, since fields shift by one extra token
when a frame is lost -- likely an empty/malformed length-field placeholder -- but stay
ALIGNED when counted from the END of each tab-separated line): status = 2nd-from-last
field (1=lost, 0=received), sequence number = 6th-from-last field (verified strictly
monotonic, +1 every line, no gaps, across the full 200k-line file). Confirmed against the
IEEE DataPort page's own field description (receive time, frame length, sequence number,
signal level, status) and the readme's stated 200fps/100fps sampling rate.

Picked two of the dataset's distance/speed configs as hop1/hop2 (both real traces from the
SAME dataset, unlike the earlier hop1=due/packet-delivery + hop2=synthetic-illustrative
compromise): `rcv_05M_310m_0500B.txt` (5.5Mb/s, 310m, empirical loss 31.6%) and
`rcv_11M_230m_0500B.txt` (11Mb/s, 230m, empirical loss 72.5%) -- picked for having
substantial, non-degenerate loss rates (a near-zero-loss config, like rcv_11M_170m_0500B.txt
at 0.67%, would give an uninformative near-all-Good fit, mirroring the earlier project's own
"pick non-trivial loss rate" convention from real_trace_ge_fit_demo.py).

Run with: uv run python isti_rural_block_fit_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, ge_fit, switching_curves, warm_standby

HOP1_FILE = "data/isti_rural/rcv_05M_310m_0500B.txt"  # 5.5Mb/s, 310m, ~31.6% loss
HOP2_FILE = "data/isti_rural/rcv_11M_230m_0500B.txt"  # 11Mb/s, 230m, ~72.5% loss
DT_NATIVE_MS = 5.0  # readme: 5ms inter-transmission time at 11/5.5/2 Mb/s

T_EPOCH_S = 3.0  # rust-core isekai-transport/src/path_health.rs:52
COST_A = 0.16
C_SWITCH_WARM = 0.01
RESOLUTION = 60
N_ITERS = 2000


def parse_loss_sequence(path: str) -> np.ndarray:
    """Extracts the binary loss sequence in strict sequence-number order.

    Reads raw lines, skips comment/blank lines, and pulls status/seq by counting
    tab-separated fields from the END of the line (see module docstring for why:
    lost-frame lines have one extra leading token compared to received-frame lines).
    """
    seqs = []
    statuses = []
    with open(path) as f:
        for line in f:
            if line.startswith("#") or not line.strip():
                continue
            fields = line.rstrip("\n").split("\t")
            n = len(fields)
            status = int(fields[n - 2])
            seq = int(fields[n - 6])
            seqs.append(seq)
            statuses.append(status)
    seqs = np.asarray(seqs)
    statuses = np.asarray(statuses, dtype=float)
    order = np.argsort(seqs, kind="stable")
    seqs_sorted = seqs[order]
    assert np.all(np.diff(seqs_sorted) == 1), "sequence numbers must be strictly consecutive"
    return statuses[order]


def block_fit(path: str, label: str) -> tuple[channels.HopParams, float]:
    seq = parse_loss_sequence(path)
    empirical_loss = seq.mean()
    n_trials, k_loss = ge_fit.bin_to_windows(seq, DT_NATIVE_MS, T_EPOCH_S)

    # Genuine 30-start EM, picking the highest-log-likelihood result -- NOT a single fit with
    # an arbitrary seed. This project found (2026-07-19, due/packet-delivery hop1) that this
    # EM can converge to distinct local optima with meaningfully different fitted lambda
    # depending on initialization; only comparing log-likelihoods identifies the better one.
    best, all_starts = ge_fit.fit_ge_binomial_em_multistart(n_trials, k_loss, n_starts=30)
    lam = 1 - best.p_gb - best.p_bg
    logliks = sorted({round(ll, 2) for ll, _ in all_starts}, reverse=True)
    lams_at_logliks = {round(ll, 2): 1 - f.p_gb - f.p_bg for ll, f in all_starts}

    print(f"=== {label} ({path}) ===")
    print(f"  {len(seq)} frames, empirical loss rate={empirical_loss:.3f}, "
          f"{len(n_trials)} windows @ {T_EPOCH_S}s ({len(n_trials) * T_EPOCH_S:.0f}s recording)")
    print(f"  BEST fit (of 30 random starts, highest loglik={logliks[0]:.2f}): "
          f"p_gb={best.p_gb:.4f}, p_bg={best.p_bg:.4f}, eps_good={best.eps_good:.4f}, "
          f"eps_bad={best.eps_bad:.4f}, lambda={lam:+.3f}")
    if len(logliks) > 1:
        print(f"  other local optima found across the 30 starts (loglik: lambda): "
              + ", ".join(f"{ll:.2f}: {lams_at_logliks[ll]:+.3f}" for ll in logliks[1:4]))
    else:
        print("  all 30 starts converged to this same optimum.")

    hop = channels.HopParams(p_gb=best.p_gb, p_bg=best.p_bg, eps_good=best.eps_good, eps_bad=best.eps_bad)
    return hop, lam


def gain(hop1: channels.HopParams, hop2: channels.HopParams, c_switch_cold: float, c_warm: float = 0.02) -> float:
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, c_warm, C_SWITCH_WARM, c_switch_cold)
    sol_adapt = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_warm = switching_curves.always_warm_value_iteration(hop1, hop2, COST_A, c_warm, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(hop1, hop2, COST_A, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    baseline = min(sol_warm.g, sol_cold.g)
    return (baseline - sol_adapt.g) / baseline if baseline > 0 else 0.0


def main() -> None:
    hop1, lam1 = block_fit(HOP1_FILE, "hop1")
    print()
    hop2, lam2 = block_fit(HOP2_FILE, "hop2")

    print("\n=== Corrected real-pair gain sweep (BOTH hops now from real isti/rural block-fits) ===")
    c_switch_values = [0.017, 0.033, 0.067, 0.1, 0.167, 0.333, 0.667, 1.333, 2.67]
    print(f"{'c_switch':>10} {'gain':>10}")
    gains = []
    for cs in c_switch_values:
        g = gain(hop1, hop2, cs)
        gains.append(g)
        print(f"{cs:>10.3f} {g * 100:>9.2f}%")

    print("\n=== Verdict ===")
    print(f"hop1 lambda={lam1:+.3f}, hop2 lambda={lam2:+.3f} -- both from real ~1000s recordings,")
    print("~330 windows each (vs. due/packet-delivery's uninformative 12-36).")
    max_gain = max(gains)
    above_5pct = [c for c, g in zip(c_switch_values, gains) if g > 0.05]
    if max_gain > 0.05:
        print(f"Peak gain {max_gain*100:.1f}% -- decomposition clears the 5% bar for c_switch in "
              f"{above_5pct if above_5pct else '(none)'}.")
    else:
        print(f"Peak gain {max_gain*100:.1f}% -- does NOT clear the 5% bar anywhere in the swept range.")


if __name__ == "__main__":
    main()
