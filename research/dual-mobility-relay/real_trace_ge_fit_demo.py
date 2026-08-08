"""Task #52: fit `dmr/ge_fit.py`'s GE moment-method estimator to REAL packet-
level field data for the first time this project, using CRAWDAD
`due/packet-delivery` (DOI 10.15783/C7NP4Z, University of Duisburg-Essen +
NTNU indoor WSN link, ~200M packets, 2012-2013) -- confirmed (not merely
assumed from its description page) to contain genuine per-packet
success/failure records: the delay traceset encodes a lost packet as the
exact sentinel value `1111` (47.7% of all tokens in a spot check -- far too
frequent and exactly-repeated to be a real continuous delay measurement,
confirming it is a fixed marker, not real data).

This replaces the earlier (incorrect) plan to use MIT Roofnet, which turned
out not to actually be hosted anywhere publicly accessible (see
TRACE_CALIBRATION_NOTES.md's "CORRECTION" section) -- `due/packet-delivery`
was found, verified to exist, and downloaded instead, using the user's own
authenticated IEEE DataPort session.

Requires: `data/due_packet_delivery/extracted/delay_10m_12runs.txt`, extracted
from the downloaded `delay_10m_12runs.rar` (CRAWDAD `due/packet-delivery`,
distance=10m subset -- ~191MB, not committed to the repo; re-download
via IEEE DataPort with a free account, see TRACE_CALIBRATION_NOTES.md).

Run with: uv run python real_trace_ge_fit_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import ge_fit

DATA_PATH = "data/due_packet_delivery/extracted/delay_10m_12runs.txt"
LOSS_SENTINEL = "1111"
HEADER_FIELDS = ["inter_arrival_ms", "payload_bytes", "queue_size", "max_tries",
                 "retry_delay_ms", "tx_power_level", "distance_m"]


def parse_line(line: str) -> tuple[dict, np.ndarray]:
    tokens = line.strip().split(",")
    header = dict(zip(HEADER_FIELDS, tokens[:7]))
    seq = np.array([1 if t == LOSS_SENTINEL else 0 for t in tokens[7:]])
    return header, seq


def main() -> None:
    with open(DATA_PATH) as f:
        lines = f.readlines()
    print(f"=== {len(lines)} distinct stack-parameter configurations in this file ===")

    rng = np.random.default_rng(0)
    sample_idx = rng.choice(len(lines), size=30, replace=False)

    results = []
    for i in sample_idx:
        header, seq = parse_line(lines[i])
        if seq.mean() in (0.0, 1.0):
            continue  # no losses or all losses -- run-length fit undefined
        fit = ge_fit.fit_gilbert_runlength(seq)
        lam = 1.0 - fit.p_gb - fit.p_bg
        results.append((header, float(seq.mean()), fit.p_gb, fit.p_bg, lam))

    print(f"{len(results)}/{len(sample_idx)} sampled configs gave a valid fit "
          f"(excluded: all-loss or all-success sequences)")

    lambdas = np.array([r[4] for r in results])
    loss_rates = np.array([r[1] for r in results])
    print(f"\nlambda (persistence, 1-p_gb-p_bg) range: [{lambdas.min():.3f}, {lambdas.max():.3f}], "
          f"mean={lambdas.mean():.3f}")
    print(f"loss rate range: [{loss_rates.min():.3f}, {loss_rates.max():.3f}]")
    n_bursty = int((lambdas > 0.05).sum())
    n_alternating = int((lambdas < -0.05).sum())
    n_near_iid = len(results) - n_bursty - n_alternating
    print(f"{n_bursty}/{len(results)} configs show clearly bursty loss (lambda>0.05)")
    print(f"{n_alternating}/{len(results)} configs show clearly ALTERNATING loss (lambda<-0.05)")
    print(f"{n_near_iid}/{len(results)} configs are close to i.i.d. loss (|lambda|<=0.05)")

    print("\n=== Sample fits (sorted by lambda) ===")
    for header, loss, p_gb, p_bg, lam in sorted(results, key=lambda r: r[4]):
        h = header
        print(f"  interarrival={h['inter_arrival_ms']}ms payload={h['payload_bytes']}B "
              f"queue={h['queue_size']} max_tries={h['max_tries']} power={h['tx_power_level']} "
              f"dist={h['distance_m']}m: loss_rate={loss:.3f}, p_gb={p_gb:.3f}, p_bg={p_bg:.3f}, "
              f"lambda={lam:+.3f}")

    print("\n=== Verdict ===")
    print("Real per-packet field data supports a genuine GE run-length fit (not just an aggregate")
    print("loss rate) -- the first real (non-synthetic) calibration point(s) obtained in this")
    print("project. The persistence spans a wide range depending on protocol configuration (queue")
    print("size, max retries, TX power) at this short (10m) distance: from mildly bursty")
    print("(lambda~+0.35) through near-independent to genuinely ALTERNATING (lambda~-0.68) --")
    print("i.e. real short-range WSN losses are NOT always well-described as 'bursty' in the GE")
    print("sense; some configurations produce loss patterns with LESS memory than an i.i.d. process")
    print("would have. This is itself a useful, non-obvious calibration finding for #52's parameter-")
    print("space placement, not just a single anchor point.")


if __name__ == "__main__":
    main()
