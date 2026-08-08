"""Per opus-symbolic-advisor's second polish suggestion (2026-07-19): before claiming the
+/-20% perturbation box in `warm_cold_robustness_sweep_demo.py` is "conservative" relative to
real calibration uncertainty, actually check it against an empirical estimate of that uncertainty
-- rather than asserting it without evidence.

Runs a block bootstrap (block size chosen to roughly match this project's already-established
short-range autocorrelation decay, see `berlin_v2x_block_fit_demo.py`'s own lag-1..lag20
autocorrelation printout) over each hop's per-epoch (n_trials, k_loss) window sequence, refitting
the Binomial-HMM EM (fewer multistarts than the main fit, for speed -- this is a rough uncertainty
estimate, not a replacement for the main calibration) on each resample, and reports the empirical
spread of (p_gb, p_bg, eps_good, eps_bad) as a fraction of each parameter's point estimate, to
compare directly against the +/-20% perturbation width used in the robustness sweep.

Run with: uv run python berlin_v2x_bootstrap_ci_demo.py  (~5-8 min)
"""

from __future__ import annotations

import numpy as np

from dmr import ge_fit

HOP1_FILE = "data/berlin_v2x/berlin_v2x_seg71.npz"
HOP2_FILE = "data/berlin_v2x/berlin_v2x_seg98.npz"
T_EPOCH_S = 3.0
DT_NATIVE_S = 1.0

N_BOOTSTRAP = 8
N_STARTS = 6
BLOCK_SIZE = 20  # windows per block; autocorrelation decays to near-noise by lag~10-20 (see
                 # berlin_v2x_block_fit_demo.py's own printout), so 20-window blocks preserve
                 # the short-range dependence structure while still allowing genuine resampling
SEED = 20260719


def load_per_second_arrays(path: str) -> tuple[np.ndarray, np.ndarray]:
    data = np.load(path)
    n_trials, k_loss = data["n_trials"], data["k_loss"]
    return n_trials, np.clip(k_loss, 0, n_trials)


def bin_prebinned_to_windows(n_trials_1s, k_loss_1s, t_window_s):
    seconds_per_window = int(round(t_window_s / DT_NATIVE_S))
    n_full = len(n_trials_1s) // seconds_per_window
    n_trimmed = n_trials_1s[: n_full * seconds_per_window].reshape(n_full, seconds_per_window)
    k_trimmed = k_loss_1s[: n_full * seconds_per_window].reshape(n_full, seconds_per_window)
    return n_trimmed.sum(axis=1), k_trimmed.sum(axis=1)


def block_bootstrap_resample(n_trials: np.ndarray, k_loss: np.ndarray, rng: np.random.Generator,
                              block_size: int) -> tuple[np.ndarray, np.ndarray]:
    n_windows = len(n_trials)
    n_blocks_needed = int(np.ceil(n_windows / block_size))
    max_start = n_windows - block_size
    starts = rng.integers(0, max_start + 1, size=n_blocks_needed)
    n_out, k_out = [], []
    for s in starts:
        n_out.append(n_trials[s:s + block_size])
        k_out.append(k_loss[s:s + block_size])
    n_out = np.concatenate(n_out)[:n_windows]
    k_out = np.concatenate(k_out)[:n_windows]
    return n_out, k_out


def bootstrap_ci(path: str, label: str) -> None:
    n_1s, k_1s = load_per_second_arrays(path)
    n_trials, k_loss = bin_prebinned_to_windows(n_1s, k_1s, T_EPOCH_S)

    rng = np.random.default_rng(SEED)
    point, _ = ge_fit.fit_ge_binomial_em_multistart(n_trials, k_loss, n_starts=N_STARTS)
    print(f"=== {label} bootstrap ({N_BOOTSTRAP} resamples, block={BLOCK_SIZE} windows, "
          f"n_starts={N_STARTS}) ===")
    print(f"  point estimate: p_gb={point.p_gb:.4f}, p_bg={point.p_bg:.4f}, "
          f"eps_good={point.eps_good:.4f}, eps_bad={point.eps_bad:.4f}")

    boot = {"p_gb": [], "p_bg": [], "eps_good": [], "eps_bad": []}
    for b in range(N_BOOTSTRAP):
        n_b, k_b = block_bootstrap_resample(n_trials, k_loss, rng, BLOCK_SIZE)
        fit, _ = ge_fit.fit_ge_binomial_em_multistart(n_b, k_b, n_starts=N_STARTS)
        boot["p_gb"].append(fit.p_gb)
        boot["p_bg"].append(fit.p_bg)
        boot["eps_good"].append(fit.eps_good)
        boot["eps_bad"].append(fit.eps_bad)
        print(f"  [{b+1}/{N_BOOTSTRAP}] p_gb={fit.p_gb:.4f}, p_bg={fit.p_bg:.4f}, "
              f"eps_good={fit.eps_good:.4f}, eps_bad={fit.eps_bad:.4f}")

    print(f"\n  {'param':>10}  {'point':>8}  {'boot std':>9}  {'std/point %':>12}  {'range/point %':>14}")
    for k in boot:
        vals = np.array(boot[k])
        pt = getattr(point, k)
        std = vals.std()
        rng_frac = (vals.max() - vals.min()) / pt * 100 if pt > 0 else float("nan")
        std_frac = std / pt * 100 if pt > 0 else float("nan")
        print(f"  {k:>10}  {pt:>8.4f}  {std:>9.4f}  {std_frac:>11.1f}%  {rng_frac:>13.1f}%")
    print()


def main() -> None:
    bootstrap_ci(HOP1_FILE, "hop1 (car4->car2)")
    bootstrap_ci(HOP2_FILE, "hop2 (car3->car1)")
    print("Compare 'std/point %' and 'range/point %' above against the +/-20% perturbation "
          "width used in warm_cold_robustness_sweep_demo.py.")


if __name__ == "__main__":
    main()
