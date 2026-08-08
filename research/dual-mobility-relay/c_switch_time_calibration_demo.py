"""Real-timing calibration of c_switch/c_warm, per the 2026-07-19 Opus/Codex-style
consultation on what to do after item 15's homotopy counterexample (the single-crossing
conjecture is dead; the project's stated purpose is informing Stage 1 of the PARENT
isekai-terminal codebase's actual QUIC multipath engineering, not more theory).

REAL NUMBERS FOUND IN rust-core (2026-07-19, verified directly, not just an agent's claim):
  - `HEALTH_CHECK_INTERVAL = 3s` (isekai-transport/src/path_health.rs:52) -- the real
    system's actual route re-classification cadence. This IS the decision epoch: the
    controller cannot re-decide routing faster than it re-scores path health. Chosen over
    the GE channel's own dwell time as the epoch definition, since dwell time is a channel
    property (how fast the world moves), not a decision cadence (how often the controller
    acts) -- conflating them was flagged as a mistake to avoid.
  - `OPEN_PATH_TIMEOUT = 8s` (isekai-transport/src/multipath.rs:45) -- an empirically
    confirmed real-device (Android, cellular) FAILURE timeout for path validation, not the
    duration of a successful migration. Used here only as the pessimistic "failure corner"
    upper bound, explicitly NOT labeled as "typical".
  - No real measured duration for a SUCCESSFUL PATH_CHALLENGE/PATH_RESPONSE round trip
    exists in this codebase. The optimistic end of the range used below (~1 RTT, 0.05-0.2s)
    is a theoretical estimate (typical cellular RTT), not a measured number -- stated
    honestly as such.

THE BIGGER FINDING (discovered while building this calibration, not assumed going in) --
RETRACTED 2026-07-19, see TRACE_CALIBRATION_NOTES.md's "CORRECTION" section, kept here (struck
through in substance, not deleted) so the code's own history matches the notes' paper trail:
  the two real due/packet-delivery hop fits used throughout this project's #52 real-data
  work (p_gb=0.0664/p_bg=0.6526 @ inter_arrival_ms=30, and p_gb=0.0787/p_bg=0.9155 @
  inter_arrival_ms=10) were fit at native per-packet sampling of 10-30ms. Resampled to the
  real system's 3s decision epoch via `ge_fit.resample_gilbert_to_epoch`'s closed-form
  lambda^k rescaling, both hops' resampled persistence prints as 0.00e+00 in this script's
  output (hop2 genuinely underflows, `0.0058^300~1e-671`; hop1 is `0.281^100~7.4e-56`, NOT
  "<1e-100" as an earlier version of this comment claimed -- still negligible for the MDP, but
  the magnitude claim was numerically wrong, per an Opus-model review). More importantly, a
  separate Fable-model review found the UPSTREAM per-packet pure-Gilbert fit itself
  (`fit_gilbert_runlength`, assumes eps_bad=1) is invalidated by these traces' own second-order
  statistics -- real multi-second partial-loss fades get chopped into short artificial "bad
  runs", catastrophically underestimating persistence; refitting at 3s-block granularity gives
  hop1 lambda(3s)~0.24-0.44, not ~0. **Do not cite "this real channel pair has zero
  decomposition value at 3s regardless of c_switch" -- it is not supported.** Also do NOT
  repeat "k=100/300 is far more than enough to decay any lambda<1 away" as a general
  justification (false: `0.99**100=0.366`, not decayed) -- these two hops' tiny resampled
  lambda is because their *fitted native* lambda was already low, not because k was
  universally large enough; a slower-fading real link would NOT decorrelate the same way.
  See TRACE_CALIBRATION_NOTES.md for the full correction and the corrected next step
  (refit at block granularity, or full EM, before drawing any real-channel conclusion).

Caveat stated once, not repeated per-line: `due/packet-delivery` is an indoor WSN lab
benchmark (10-35m), not the drone-relay-vehicle scenario's actual RF link -- a real
calibration data point, but from a different physical environment (already flagged in
TRACE_CALIBRATION_NOTES.md). The qualitative lesson (native channel memory can decay much
faster than a practical decision cadence) is still an honest, general engineering caveat for
Stage 1, even though this specific WSN dataset's absolute burst-length numbers may not
transfer directly to Wi-Fi/UHF drone links.

Two sweeps:
  A) PRIMARY (answers Opus's ask): fix T_epoch=3s (the real HEALTH_CHECK_INTERVAL), vary
     c_switch across the physically-plausible range (T_switch in [0.05s, 8s] / T_epoch,
     disruption_frac in {0.5, 1.0}) using a SYNTHETIC hop pair with non-trivial persistence
     at the 3s epoch (since the real hop pair's is exactly 0, sweep A over it would be a
     flat, uninformative zero-line -- see sweep A's own printed note). This isolates "does
     c_switch matter, holding channel memory fixed" the way the original question asked.
  B) THE EPOCH-SENSITIVITY FINDING: using the REAL hop pair, sweep T_epoch itself from the
     channel's own native timescale (10ms) up past the real system's 3s to 30s, holding
     c_switch fixed (mid-range), to show gain collapsing as epoch grows past the channel's
     own persistence timescale -- the boundary this project's own real 3s constant sits well
     past.

Run with: uv run python c_switch_time_calibration_demo.py
"""

from __future__ import annotations

import numpy as np

from dmr import beliefgrid2d, channels, ge_fit, switching_curves, warm_standby

# Real hop fits from real_trace_ge_fit_demo.py / real_channel_adaptivity_sweep_demo.py
# (due/packet-delivery, distance=10m), with their native per-packet sampling interval.
REAL_HOP1_NATIVE = dict(p_gb=0.0664, p_bg=0.6526, dt_native_ms=30.0)
REAL_HOP2_NATIVE = dict(p_gb=0.0787, p_bg=0.9155, dt_native_ms=10.0)

COST_A = 0.16
C_SWITCH_WARM = 0.01
RESOLUTION = 60
N_ITERS = 2000

REAL_HEALTH_CHECK_INTERVAL_S = 3.0  # rust-core isekai-transport/src/path_health.rs:52
FAILURE_TIMEOUT_S = 8.0  # rust-core isekai-transport/src/multipath.rs:45 (failure corner, not typical)
OPTIMISTIC_SWITCH_S = (0.05, 0.2)  # theoretical ~1 RTT range, NOT a measured number


def decomposition_gain(hop1: channels.HopParams, hop2: channels.HopParams, c_switch_cold: float) -> float:
    """Relative value (Eq. 9 from the paper's section 8.6): (min(g_warm,g_cold)-g_adapt)/min(g_warm,g_cold)."""
    path_b_loss = channels.path_b_loss_prob(hop1, hop2)
    c_warm = 0.02  # fixed at the paper's own near-peak-sensitivity value; c_warm is a secondary axis here
    cost = warm_standby.cost_with_warm_standby(path_b_loss, COST_A, c_warm, C_SWITCH_WARM, c_switch_cold)
    sol_adapt = beliefgrid2d.belief_grid2d_value_iteration_warm(hop1, hop2, cost, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_warm = switching_curves.always_warm_value_iteration(hop1, hop2, COST_A, c_warm, C_SWITCH_WARM, resolution=RESOLUTION, n_iters=N_ITERS)
    sol_cold = switching_curves.always_cold_value_iteration(hop1, hop2, COST_A, c_switch_cold, resolution=RESOLUTION, n_iters=N_ITERS)
    baseline = min(sol_warm.g, sol_cold.g)
    return (baseline - sol_adapt.g) / baseline if baseline > 0 else 0.0


def sweep_a_c_switch_at_real_epoch() -> None:
    print("=== Sweep A: gain vs c_switch, fixed at the real HEALTH_CHECK_INTERVAL=3s epoch ===\n")

    p_gb1, p_bg1 = ge_fit.resample_gilbert_to_epoch(**REAL_HOP1_NATIVE, t_epoch_s=REAL_HEALTH_CHECK_INTERVAL_S)
    p_gb2, p_bg2 = ge_fit.resample_gilbert_to_epoch(**REAL_HOP2_NATIVE, t_epoch_s=REAL_HEALTH_CHECK_INTERVAL_S)
    lam1, lam2 = 1 - p_gb1 - p_bg1, 1 - p_gb2 - p_bg2
    print(f"real hop pair resampled to 3s epoch: hop1 lambda={lam1:.2e}, hop2 lambda={lam2:.2e}")
    print("(both collapse to ~0 -- see module docstring. A flat, uninformative zero-line for every")
    print(" c_switch would follow, so this sweep instead uses a SYNTHETIC hop pair with non-trivial")
    print(" persistence AT the 3s epoch, to isolate the c_switch question the way it was originally")
    print(" asked -- 'holding channel memory fixed, does switch cost matter'. The real hop pair's")
    print(" own answer is the separate, more fundamental finding in Sweep B below.)\n")

    # Synthetic pair with real, non-trivial persistence AT a 3s epoch (unlike the real WSN pair,
    # whose native bursts are too fast to survive resampling to this cadence) -- same eps_good/
    # eps_bad=0/1 pure-Gilbert structure as the real data, same cost_a, for comparability.
    hop1 = channels.HopParams(p_gb=0.05, p_bg=0.15, eps_good=0.0, eps_bad=1.0)  # lambda=+0.80 at 3s
    hop2 = channels.HopParams(p_gb=0.02, p_bg=0.30, eps_good=0.0, eps_bad=1.0)  # lambda=+0.68 at 3s
    print(f"synthetic 3s-epoch pair: hop1 lambda={1-hop1.p_gb-hop1.p_bg:+.2f}, hop2 lambda={1-hop2.p_gb-hop2.p_bg:+.2f}\n")

    t_switch_values = np.array([0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 4.0, 8.0])
    print(f"{'T_switch(s)':>12} {'disruption=1.0':>16} {'disruption=0.5':>16} {'c_switch(1.0)':>14} {'c_switch(0.5)':>14}")
    crossing_5pct = {1.0: None, 0.5: None}
    crossing_10pct = {1.0: None, 0.5: None}
    for t_switch in t_switch_values:
        row = [f"{t_switch:>12.2f}"]
        c_switches = {}
        for disruption in (1.0, 0.5):
            c_switch = disruption * t_switch / REAL_HEALTH_CHECK_INTERVAL_S
            c_switches[disruption] = c_switch
            gain = decomposition_gain(hop1, hop2, c_switch)
            row.append(f"{gain*100:>15.2f}%")
            if crossing_5pct[disruption] is None and gain < 0.05:
                crossing_5pct[disruption] = t_switch
            if crossing_10pct[disruption] is None and gain < 0.10:
                crossing_10pct[disruption] = t_switch
        row.append(f"{c_switches[1.0]:>14.3f}")
        row.append(f"{c_switches[0.5]:>14.3f}")
        print(" ".join(row))

    print(f"\noptimistic successful-switch range: T_switch in [{OPTIMISTIC_SWITCH_S[0]}, {OPTIMISTIC_SWITCH_S[1]}]s "
          f"(theoretical ~1 RTT, NOT measured) -> c_switch in "
          f"[{OPTIMISTIC_SWITCH_S[0]/REAL_HEALTH_CHECK_INTERVAL_S:.3f}, {OPTIMISTIC_SWITCH_S[1]/REAL_HEALTH_CHECK_INTERVAL_S:.3f}]")
    print(f"failure-corner upper bound (measured, real device): T_switch={FAILURE_TIMEOUT_S}s -> "
          f"c_switch={FAILURE_TIMEOUT_S/REAL_HEALTH_CHECK_INTERVAL_S:.2f} (NOT typical -- this is the cost of a FAILED attempt)")
    print(f"\ngain crosses below 5% at T_switch ~= {crossing_5pct} seconds (disruption_frac keyed)")
    print(f"gain crosses below 10% at T_switch ~= {crossing_10pct} seconds (disruption_frac keyed)")


def sweep_b_epoch_sensitivity_real_hops() -> None:
    print("\n\n=== Sweep B: gain vs decision epoch T_epoch, REAL hop pair, c_switch held fixed ===\n")
    print("(does the real system's HEALTH_CHECK_INTERVAL=3s epoch even leave any channel memory to")
    print(" exploit, for THIS real WSN channel pair? Fixing a representative mid-range c_switch=0.1)\n")

    c_switch_cold = 0.1
    epochs_s = [0.01, 0.03, 0.1, 0.3, 1.0, 3.0, 10.0, 30.0]
    print(f"{'T_epoch(s)':>12} {'hop1 lambda':>14} {'hop2 lambda':>14} {'gain':>10}")
    for t_epoch in epochs_s:
        p_gb1, p_bg1 = ge_fit.resample_gilbert_to_epoch(**REAL_HOP1_NATIVE, t_epoch_s=t_epoch)
        p_gb2, p_bg2 = ge_fit.resample_gilbert_to_epoch(**REAL_HOP2_NATIVE, t_epoch_s=t_epoch)
        hop1 = channels.HopParams(p_gb=p_gb1, p_bg=p_bg1, eps_good=0.0, eps_bad=1.0)
        hop2 = channels.HopParams(p_gb=p_gb2, p_bg=p_bg2, eps_good=0.0, eps_bad=1.0)
        lam1, lam2 = 1 - p_gb1 - p_bg1, 1 - p_gb2 - p_bg2
        gain = decomposition_gain(hop1, hop2, c_switch_cold)
        marker = "  <- real HEALTH_CHECK_INTERVAL" if t_epoch == REAL_HEALTH_CHECK_INTERVAL_S else ""
        print(f"{t_epoch:>12.2f} {lam1:>14.2e} {lam2:>14.2e} {gain*100:>9.3f}%{marker}")

    print("\n=== Verdict -- RETRACTED, 2026-07-19, see TRACE_CALIBRATION_NOTES.md 'CORRECTION' ===")
    print("This sweep's gain-vs-epoch numbers above are computed correctly FROM the upstream")
    print("per-packet pure-Gilbert fits (REAL_HOP1_NATIVE/REAL_HOP2_NATIVE), but a Fable-model")
    print("review found those upstream fits themselves are invalidated by the raw traces' own")
    print("second-order statistics: real multi-second partial-loss fades get chopped into short")
    print("artificial 'bad runs' by fit_gilbert_runlength's eps_bad=1 assumption, catastrophically")
    print("underestimating persistence. Refitting at 3s-block granularity instead gives hop1")
    print("lambda(3s)~0.24-0.44, not ~0 -- i.e. this real channel pair likely does NOT fully")
    print("decorrelate by the real 3s health-check cadence after all. Do NOT conclude 'gain is ~0")
    print("regardless of c_switch for this real pair' from this sweep's printed numbers -- that")
    print("headline is retracted pending a corrected block-level (or full EM) refit. What remains")
    print("valid: the resample math itself, and the qualitative point that epoch-vs-channel-")
    print("timescale mismatch is a real axis worth checking -- just not, as it turns out, one this")
    print("specific (mis-fit) real hop pair actually demonstrates.")


if __name__ == "__main__":
    sweep_a_c_switch_at_real_epoch()
    sweep_b_epoch_sensitivity_real_hops()
