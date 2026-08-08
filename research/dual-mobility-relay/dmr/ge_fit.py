"""Task #61: a from-scratch numpy moment-method estimator fitting a
Gilbert-Elliott channel to a binary (loss/success) sequence, per the
decision recorded in TRACE_CALIBRATION_NOTES.md (task #60): a hand-rolled
run-length-based fitter is adequate for the pure-Gilbert special case
(`eps_good=0`, `eps_bad=1` -- loss is a deterministic function of the
hidden state), keeping this project's numpy/scipy/matplotlib-only
dependency policy (`pyproject.toml`). Baum-Welch/`hmmlearn` is NOT
implemented here -- it would only be needed for the full Gilbert-Elliott
case (`eps_good>0`, `eps_bad<1`), which is underdetermined by the simple
moments used below (see this module's docstring on `fit_gilbert_elliott_moments`
for exactly why) and is out of scope for placing points on a parameter-
space map.

Two estimators:
  - `fit_gilbert_runlength`: pure-Gilbert (loss deterministic by state) --
    exactly solvable from mean good/bad run lengths, no ambiguity.
  - `fit_gilbert_elliott_moments`: attempts the general case via marginal
    loss rate + lag-1/lag-2 autocovariance, but is explicitly flagged as
    UNDERDETERMINED (returns `None` for eps_good/eps_bad, only p_gb+p_bg is
    identified) -- included to document precisely where the moment method
    stops being sufficient, not as a working general-case fitter.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np


@dataclass(frozen=True)
class GilbertFit:
    p_gb: float
    p_bg: float
    n_good_runs: int
    n_bad_runs: int
    mean_good_run_length: float
    mean_bad_run_length: float
    empirical_loss_rate: float


def _run_lengths(sequence: np.ndarray, value: int) -> np.ndarray:
    """Lengths of maximal contiguous runs of `value` in a 0/1 sequence."""
    is_val = (sequence == value).astype(int)
    if is_val.sum() == 0:
        return np.array([])
    padded = np.concatenate(([0], is_val, [0]))
    diff = np.diff(padded)
    starts = np.where(diff == 1)[0]
    ends = np.where(diff == -1)[0]
    return (ends - starts).astype(float)


def fit_gilbert_runlength(loss_sequence: np.ndarray) -> GilbertFit:
    """Fits the pure-Gilbert model (state Bad => loss with probability 1,
    state Good => loss with probability 0) to a binary loss sequence via
    run-length statistics -- exactly solvable, no EM needed:

      mean_bad_run_length  = 1 / p_bg   (geometric sojourn in Bad)
      mean_good_run_length = 1 / p_gb   (geometric sojourn in Good)

    `loss_sequence` must be a 1D array of 0 (success) / 1 (loss).
    """
    loss_sequence = np.asarray(loss_sequence)
    if not np.all(np.isin(loss_sequence, [0, 1])):
        raise ValueError("loss_sequence must be binary (0=success, 1=loss)")

    good_runs = _run_lengths(loss_sequence, 0)
    bad_runs = _run_lengths(loss_sequence, 1)
    if len(good_runs) == 0 or len(bad_runs) == 0:
        raise ValueError("sequence must contain at least one good run and one bad run")

    mean_good = float(good_runs.mean())
    mean_bad = float(bad_runs.mean())
    p_gb = 1.0 / mean_good
    p_bg = 1.0 / mean_bad

    return GilbertFit(
        p_gb=p_gb, p_bg=p_bg,
        n_good_runs=len(good_runs), n_bad_runs=len(bad_runs),
        mean_good_run_length=mean_good, mean_bad_run_length=mean_bad,
        empirical_loss_rate=float(loss_sequence.mean()),
    )


def fit_gilbert_elliott_moments(loss_sequence: np.ndarray) -> dict:
    """Attempts a moment-method fit of the FULL Gilbert-Elliott model
    (loss possible in both states) via marginal rate + autocovariance decay.
    Documents exactly where this is underdetermined, rather than silently
    returning a wrong point estimate.

    For k>=1, `Cov(x_t, x_{t+k}) = pi_good*pi_bad*(eps_bad-eps_good)^2 *
    lambda^k` where `lambda = 1 - p_gb - p_bg` (the transition matrix's
    second eigenvalue) and `pi_bad = p_gb/(p_gb+p_bg)` is the stationary
    P(Bad) -- the within-state Bernoulli emission noise only contributes to
    lag-0 variance, not to any lag>=1 covariance, since consecutive same-
    state draws are conditionally independent given the state path.

    This gives `lambda` (hence `p_gb+p_bg`) from `Cov(2)/Cov(1)`, and the
    single combined quantity `pi_good*pi_bad*(eps_bad-eps_good)^2` from
    `Cov(1)/lambda`. That is only 2 independent equations (plus the marginal
    rate `q = pi_bad*eps_bad + pi_good*eps_good`, a 3rd) for 4 unknowns
    (`p_gb`, `p_bg`, `eps_good`, `eps_bad`) -- individually splitting
    `p_gb`/`p_bg` (only their sum is pinned) and `eps_good`/`eps_bad` (only
    a product-like combination is pinned) needs a 4th independent moment
    (e.g. the bad-run-length distribution's shape, or lag-3+ information
    beyond the geometric decay already captured by `lambda`) -- which is
    exactly the job Baum-Welch/EM does by using the full likelihood instead
    of a handful of moments. Returns the identifiable quantities and `None`
    for the rest, rather than fabricating a resolution the data doesn't
    support.
    """
    x = np.asarray(loss_sequence, dtype=float)
    n = len(x)
    q = float(x.mean())
    x_centered = x - q

    def autocov(k: int) -> float:
        return float(np.mean(x_centered[: n - k] * x_centered[k:]))

    cov0, cov1, cov2 = autocov(0), autocov(1), autocov(2)
    if cov1 == 0:
        raise ValueError("lag-1 autocovariance is zero -- sequence shows no Markov structure to fit")

    lam = cov2 / cov1
    p_sum = 1.0 - lam  # p_gb + p_bg

    return {
        "lambda": lam,
        "p_gb_plus_p_bg": p_sum,
        "between_state_variance_x_lambda": cov1,  # pi_good*pi_bad*(eps_bad-eps_good)^2 * lambda
        "marginal_loss_rate": q,
        "p_gb": None,
        "p_bg": None,
        "eps_good": None,
        "eps_bad": None,
        "note": ("Underdetermined by lag-1/lag-2 moments alone: only p_gb+p_bg and a combined "
                 "pi_good*pi_bad*(eps_bad-eps_good)^2*lambda term are identified. Splitting p_gb "
                 "from p_bg and eps_good from eps_bad needs Baum-Welch/EM (out of scope, see module "
                 "docstring) or the pure-Gilbert special case (fit_gilbert_runlength)."),
    }


def resample_gilbert_to_epoch(p_gb: float, p_bg: float, dt_native_ms: float, t_epoch_s: float) -> tuple[float, float]:
    """Re-express a GE hop's (p_gb, p_bg), fitted at native sampling interval
    `dt_native_ms`, as the equivalent (p_gb', p_bg') at a coarser decision
    epoch `t_epoch_s` -- i.e. the transition probabilities a decision-maker
    who only observes/acts once every `t_epoch_s` seconds would see, if the
    underlying continuous-time process is unchanged.

    Closed form for a 2-state chain: the persistence `lambda = 1-p_gb-p_bg`
    is the transition matrix's second eigenvalue, so composing k native
    steps gives `lambda_k = lambda^k` exactly (eigenvalues of a matrix power
    are powers of the eigenvalues), while the stationary P(Bad) =
    p_gb/(p_gb+p_bg) is preserved under any number of steps of the SAME
    chain by definition of stationarity. Recovering p_gb'/p_bg' from
    (lambda_k, pi_bad) is then just algebra: p_gb' = pi_bad*(1-lambda_k),
    p_bg' = (1-pi_bad)*(1-lambda_k).

    `k = t_epoch_s*1000 / dt_native_ms` need not be an integer (lambda^k for
    non-integer k is the natural continuous-time extension of a 2-state
    Markov chain's persistence, since a 2x2 stochastic generator's matrix
    exponential has exactly this eigenvalue form).

    If `lambda` is negative (real "alternating" loss processes can have this
    -- see TRACE_CALIBRATION_NOTES.md), `lambda^k` for non-integer k is only
    real-valued if `lambda>=0`; this function takes `abs(lambda)**k` with the
    ORIGINAL SIGN reapplied only when k is (numerically close to) an
    integer, and otherwise falls back to treating the chain as if it had
    already decorrelated (lambda_k=0) -- alternating structure at native
    resolution does not have a well-defined non-integer-step extension, and
    in practice `k` is large enough here that lambda_k is negligible either
    way (see usage in c_switch_time_calibration_demo.py).
    """
    if dt_native_ms <= 0 or t_epoch_s <= 0:
        raise ValueError("dt_native_ms and t_epoch_s must be positive")
    lam = 1.0 - p_gb - p_bg
    pi_bad = p_gb / (p_gb + p_bg)
    k = (t_epoch_s * 1000.0) / dt_native_ms
    if lam < 0:
        k_int = round(k)
        lam_k = lam ** k_int if abs(k - k_int) < 1e-6 else 0.0
    else:
        lam_k = lam ** k
    p_gb_new = pi_bad * (1.0 - lam_k)
    p_bg_new = (1.0 - pi_bad) * (1.0 - lam_k)
    return p_gb_new, p_bg_new


def bin_to_windows(loss_sequence: np.ndarray, dt_native_ms: float, t_window_s: float) -> tuple[np.ndarray, np.ndarray]:
    """Bin a per-packet binary loss sequence into fixed-duration windows,
    returning (n_trials, k_loss) per window -- the sufficient statistic for a
    Binomial-emission HMM at the window's own time scale. Windows are
    contiguous, non-overlapping, `t_window_s` seconds each; a final partial
    window (fewer packets than a full window) is dropped rather than biasing
    its emission probability with fewer trials.
    """
    loss_sequence = np.asarray(loss_sequence)
    packets_per_window = int(round((t_window_s * 1000.0) / dt_native_ms))
    if packets_per_window < 1:
        raise ValueError("t_window_s is shorter than one native sampling interval")
    n_full_windows = len(loss_sequence) // packets_per_window
    trimmed = loss_sequence[: n_full_windows * packets_per_window].reshape(n_full_windows, packets_per_window)
    n_trials = np.full(n_full_windows, packets_per_window, dtype=float)
    k_loss = trimmed.sum(axis=1).astype(float)
    return n_trials, k_loss


@dataclass(frozen=True)
class GEBinomialFit:
    p_gb: float
    p_bg: float
    eps_good: float
    eps_bad: float
    n_windows: int
    loglik_history: list


def _binomial_logpmf(k: np.ndarray, n: np.ndarray, p: np.ndarray) -> np.ndarray:
    from scipy.special import gammaln  # local import: this project's only scipy dependency besides beliefgrid.py's Delaunay use
    p = np.clip(p, 1e-12, 1 - 1e-12)
    return gammaln(n + 1) - gammaln(k + 1) - gammaln(n - k + 1) + k * np.log(p) + (n - k) * np.log(1 - p)


def fit_ge_binomial_em(n_trials: np.ndarray, k_loss: np.ndarray, n_iters: int = 200,
                        tol: float = 1e-8, seed: int = 0) -> GEBinomialFit:
    """Fits a 2-state Gilbert-Elliott hidden Markov model DIRECTLY at the
    window's own time scale, with Binomial(n_t, eps_state) emissions per
    window (n_t trials, k_t losses), via Baum-Welch EM (Rabiner 1989 scaled
    forward-backward).

    This exists specifically to avoid `fit_gilbert_runlength`'s eps_bad=1
    assumption, which was found (2026-07-19, see TRACE_CALIBRATION_NOTES.md's
    "CORRECTION" section) to catastrophically underestimate persistence for
    real partial-loss fades: a long real fade with eps_bad<1 gets chopped
    into many short per-packet "bad runs" whenever a packet happens to get
    through, which the run-length fitter then reads as low persistence. Here,
    persistence is estimated directly from how loss RATES cluster and
    transition across windows, not from run lengths of a thresholded binary
    sequence -- no ad hoc "is this window Bad" threshold is needed, since the
    EM's own posterior state probabilities play that role, softly and
    self-consistently with the fitted eps_good/eps_bad.

    Label-switching is resolved post-hoc: whichever fitted state has the
    LOWER eps is relabeled "Good" (state 0), so `p_gb`/`p_bg` always mean
    Good->Bad / Bad->Good regardless of which internal index the optimizer
    happened to converge to.
    """
    rng = np.random.default_rng(seed)
    n_trials = np.asarray(n_trials, dtype=float)
    k_loss = np.asarray(k_loss, dtype=float)
    n_obs = len(n_trials)

    # BUG FIX (2026-07-19): `seed` previously did nothing -- initialization was fully
    # deterministic (fixed eps/trans/pi below), so every "multi-seed stability check" in
    # earlier callers (this project's own real_channel_block_fit_demo.py and
    # TRACE_CALIBRATION_NOTES.md's "stable across all 10 different random initializations"
    # claim for hop1) was silently re-running the IDENTICAL computation, not actually testing
    # multi-start stability. Caught via isti_rural_block_fit_demo.py's suspiciously identical
    # "5-seed" result on real data. `seed` now genuinely randomizes the EM starting point.
    empirical_rate = k_loss.sum() / n_trials.sum()
    eps_lo = float(np.clip(empirical_rate * rng.uniform(0.2, 0.8), 1e-6, 1 - 1e-6))
    eps_hi = float(np.clip(empirical_rate * rng.uniform(1.2, 2.0), 1e-6, 1 - 1e-6))
    eps = np.array(sorted([eps_lo, eps_hi]))
    p_gb0 = rng.uniform(0.05, 0.45)
    p_bg0 = rng.uniform(0.05, 0.45)
    trans = np.array([[1 - p_gb0, p_gb0], [p_bg0, 1 - p_bg0]])
    pi = np.array([0.5, 0.5])

    loglik_history = []
    for _ in range(n_iters):
        # log emission likelihoods, shape (n_obs, 2)
        log_b = np.stack([_binomial_logpmf(k_loss, n_trials, np.full(n_obs, eps[s])) for s in range(2)], axis=1)
        b = np.exp(log_b - log_b.max(axis=1, keepdims=True))  # per-row shift for stability, rescaled by c[t] below anyway

        alpha_hat = np.zeros((n_obs, 2))
        c = np.zeros(n_obs)
        alpha_hat[0] = pi * b[0]
        c[0] = alpha_hat[0].sum()
        alpha_hat[0] /= c[0]
        for t in range(1, n_obs):
            alpha_hat[t] = (alpha_hat[t - 1] @ trans) * b[t]
            c[t] = alpha_hat[t].sum()
            alpha_hat[t] /= c[t]

        beta_hat = np.zeros((n_obs, 2))
        beta_hat[-1] = 1.0
        for t in range(n_obs - 2, -1, -1):
            beta_hat[t] = trans @ (b[t + 1] * beta_hat[t + 1]) / c[t + 1]

        gamma = alpha_hat * beta_hat
        gamma /= gamma.sum(axis=1, keepdims=True)

        xi_sum = np.zeros((2, 2))
        for t in range(n_obs - 1):
            xi_t = np.outer(alpha_hat[t], b[t + 1] * beta_hat[t + 1]) * trans / c[t + 1]
            xi_sum += xi_t

        pi = gamma[0].copy()
        # A state can transiently collapse to ~zero total posterior mass mid-optimization
        # (observed on real, highly-lossy traces -- e.g. isti_rural_block_fit_demo.py's 72.5%-
        # loss hop): guard against the resulting 0/0 the same way beliefgrid2d.py's
        # bayes_update_scalar does, rather than letting NaN silently propagate through every
        # later iteration (which it otherwise does, since NaN*anything=NaN forever after).
        gamma_state_mass = gamma[:-1].sum(axis=0, keepdims=True).T
        trans = np.divide(xi_sum, gamma_state_mass, out=np.zeros_like(xi_sum), where=gamma_state_mass > 0)
        row_sums = trans.sum(axis=1, keepdims=True)
        trans = np.divide(trans, row_sums, out=np.full_like(trans, 0.5), where=row_sums > 0)
        for s in range(2):
            denom = (gamma[:, s] * n_trials).sum()
            eps[s] = (gamma[:, s] * k_loss).sum() / denom if denom > 0 else eps[s]
        eps = np.clip(eps, 1e-6, 1 - 1e-6)

        # log_b already includes the row-max shift removed by the `b` normalization above; recompute
        # the true log-likelihood from the scaling factors c[t] (standard Rabiner-scaling identity:
        # log P(observations) = sum_t log(c[t]) + sum_t (row-max shift), the latter tracked separately).
        row_max = log_b.max(axis=1)
        loglik = np.log(c).sum() + row_max.sum()
        loglik_history.append(float(loglik))
        if len(loglik_history) > 1 and abs(loglik_history[-1] - loglik_history[-2]) < tol:
            break

    good_idx = int(np.argmin(eps))
    bad_idx = 1 - good_idx
    p_gb = trans[good_idx, bad_idx]
    p_bg = trans[bad_idx, good_idx]
    return GEBinomialFit(
        p_gb=float(p_gb), p_bg=float(p_bg),
        eps_good=float(eps[good_idx]), eps_bad=float(eps[bad_idx]),
        n_windows=n_obs, loglik_history=loglik_history,
    )


def fit_ge_binomial_em_multistart(n_trials: np.ndarray, k_loss: np.ndarray, n_starts: int = 30,
                                   n_iters: int = 300, tol: float = 1e-8) -> tuple[GEBinomialFit, list]:
    """Runs `fit_ge_binomial_em` from `n_starts` genuinely different random initializations
    (seeds 0..n_starts-1) and returns the one with the HIGHEST final log-likelihood, plus the
    full list of (loglik, fit) pairs from every start for transparency.

    Exists because a single run (even one that happens to look "stable" across several seeds)
    is not reliable evidence of having found the global optimum -- this project caught a real
    case (2026-07-19, `due/packet-delivery` hop1, see TRACE_CALIBRATION_NOTES.md) where 2-state
    Binomial-HMM EM on real data converges to one of (at least) two distinct local optima
    depending on initialization, with meaningfully different fitted `lambda` (+0.79 vs. +0.47).
    Always use this wrapper for anything going into a paper/report, not a single
    `fit_ge_binomial_em` call with an arbitrary or unexamined seed.
    """
    results = []
    for seed in range(n_starts):
        fit = fit_ge_binomial_em(n_trials, k_loss, n_iters=n_iters, tol=tol, seed=seed)
        results.append((fit.loglik_history[-1], fit))
    best = max(results, key=lambda x: x[0])
    return best[1], results


__all__ = [
    "GilbertFit", "fit_gilbert_runlength", "fit_gilbert_elliott_moments",
    "resample_gilbert_to_epoch", "bin_to_windows", "GEBinomialFit", "fit_ge_binomial_em",
    "fit_ge_binomial_em_multistart",
]
