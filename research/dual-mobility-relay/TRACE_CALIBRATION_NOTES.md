# Public wireless-trace datasets for GE-parameter calibration (task 5a)

Per the user's explicit constraint for this whole research effort — math/
simulation only, no real hardware experiments — this calibration uses only
**existing public datasets**, never new measurements. This note is task
#60's deliverable: dataset selection, license/reproducibility confirmation,
and the GE-fitter dependency decision, ahead of #61's estimator
implementation and #52's parameter-space placement.

## CORRECTION, 2026-07-18: Roofnet is NOT actually on IEEE DataPort/CRAWDAD

The table below (task #60's original finding, from a background research agent)
claimed MIT Roofnet is available as `mit/roofnet` in the CRAWDAD Collection on
IEEE DataPort. **This was independently checked and does not hold up.** Using
an authenticated IEEE DataPort session (the user's own browser session,
supplied directly), direct full-text searches on `ieee-dataport.org/datasets`
for "roofnet", "Aguayo" (the paper's lead author), and "CRAWDAD" all returned
**no matching datasets**. The CRAWDAD-to-IEEE-DataPort migration itself is
real (confirmed via `crawdad.org`'s own migration notice), but Roofnet
specifically does not appear to have been part of what migrated. The Wayback
Machine has **no archived snapshot ever existing** for `crawdad.org/mit/roofnet`
or several other plausible naming variants (`mit/mesh`, `mit/wifi`,
`roofnet`), suggesting Roofnet was likely never formally contributed to
CRAWDAD under any name, not just renamed/reindexed.

The original MIT PDOS Roofnet project page does still partially exist:
`pdos.csail.mit.edu/roofnet/` redirects to `pdos.csail.mit.edu/archive/roofnet/`,
which returns **403 Forbidden** (the directory exists server-side but its
listing isn't public — individual files might still be fetchable if a exact
filename were known, which wasn't found in the time spent). An archived 2008
snapshot of the project's own wiki mentions a "Publications" page linking to
"talks and trace data," but no live, working trace-data download link was
found via that path either in the time invested. Given the project dates to
2004 and its own 2008 archived notice already said the team had dispersed
("on sabbatical at Meraki Networks"), the practical reality is this dataset
has likely become an orphaned/unavailable artifact of an early-2000s academic
project, not something blocked merely by "needs a free account" as task #60
concluded — the corrected blocker is "may not be publicly obtainable at all
without directly contacting the original authors."

**Practical implication**: #52 remains blocked, but for a different and
somewhat worse reason than previously recorded. If real-trace GE calibration
is still wanted, the next step is either (a) directly asking the original
authors (Aguayo/Bicket/Biswas/Judd/Morris) whether any trace data survives in
an accessible form, or (b) re-opening #60's dataset selection with the other
3 candidates already researched there (DieselNet/DOME, Raca 4G/5G, WetLinks
Starlink) re-evaluated as primary candidates now that Roofnet is off the
table, accepting that none of them support a full per-packet GE fit the way
Roofnet was believed to (only aggregate-loss-rate anchor points).

## Candidates researched and decision

| Dataset | Access | License | Format | Verdict |
|---|---|---|---|---|
| MIT Roofnet (802.11b mesh) | ~~CRAWDAD `mit/roofnet` on IEEE DataPort~~ — **DOES NOT EXIST, see correction above** | — | — | **Rejected (2026-07-18 correction)** — the original selection rationale below no longer applies; kept in this row only as a record of the mistake. |
| CRAWDAD `umass/diesel` (DieselNet/DOME) | IEEE DataPort + traces.cs.umass.edu; free account | CRAWDAD terms | DTN contact-event traces only (bus pair, time, bytes, duration, GPS/speed) — no per-packet sequence. | **Rejected** — contact-level aggregates cannot support a packet-level GE fit. |
| Raca et al. Irish 4G/5G ("Beyond Throughput" / `uccmisl/5Gdataset`) | GitHub, GPL-3.0; less durable than an archive but widely cited/mirrored | GPL-3.0 | G-NetTrack Pro client KPIs at 1 sample/sec (throughput, RTT, RSRP/RSRQ/RSSI, cell context) — no per-packet loss. | **Secondary use only**: an aggregate cellular outage/loss-rate anchor point on the parameter-space map, not a GE fit (a 1s "outage" proxy from near-zero throughput would be too coarse to estimate `p_gb`/`p_bg` honestly). |
| Starlink LEO (WetLinks, `sys-uos/WetLinks`; also LENS, ki3.org.cn backbone set) | WetLinks: GitHub, open data, ~140k measurements/2 vantage points/6 months | Open | iperf3+ping every ~3 min; ping reports **aggregate PLR% per window** + RTT stats, not a per-packet sequence. | **Secondary use only**: an aggregate LEO loss-rate anchor point on the map, same caveat as the 4G/5G set. |
| **CRAWDAD `due/packet-delivery`** (2026-07-18, replaces Roofnet as primary candidate) | IEEE DataPort, DOI [10.15783/C7NP4Z](https://ieee-dataport.org/open-access/crawdad-duepacket-delivery), free account — **existence and DOI directly verified** (unlike the Roofnet entry above) | CRAWDAD terms: non-commercial research use, cite source | Indoor WSN (wireless sensor network) point-to-point link, Univ. of Duisburg-Essen + NTNU, ~200M packets over 6 months (2012-11 to 2013-11), varying 7 stack parameters (inter-arrival time, payload size, queue size, max retries, retry delay, TX power, distance 10-35m). Per-packet metadata captured at both sender and receiver (delay explicitly stated as per-packet; loss is one of the 4 measured metrics) — the description strongly suggests genuine per-packet success/failure records survive, not just a windowed ratio. `.rar` format. 2 independent citations from other papers confirm it's a real, used dataset. | **New primary candidate** — likely supports a genuine per-packet GE fit; format/exact per-packet-loss-field structure not yet confirmed by actually downloading (needs an account to check the real file contents). |
| **CRAWDAD `ucsb/meshnet`** (2026-07-18, secondary candidate) | IEEE DataPort, DOI [10.15783/C7WS3S](https://ieee-dataport.org/open-access/crawdad-ucsbmeshnet), free account — existence directly verified | CRAWDAD terms | 802.11a/b indoor mesh, UCSB campus, 20 nodes, measured 2006-04. Broadcast probes every 1s, but delivery ratio is reported per **10-second window** (10 probes/window), not per-probe — coarser than Roofnet was hoped to be, though still much finer than the Raca/WetLinks windows (1-3 min). `.tar.gz` format. | **Secondary/fallback candidate** — a genuine mesh-network dataset (structurally similar to what Roofnet was supposed to provide) but the public delivery-ratio field is windowed, not a raw per-probe binary sequence; would need the windowed-ratio moment-matching approach (mean + a binomial/beta-binomial burstiness estimate within each window) rather than the clean run-length method `due/packet-delivery` may support directly. |

**Decision, corrected 2026-07-18: Roofnet is replaced by `due/packet-delivery` as the
primary candidate for an actual per-packet GE fit**, with `ucsb/meshnet` as a fallback
if `due/packet-delivery`'s actual files turn out not to have per-packet-level loss
fields once downloaded and inspected (not yet done — this table's assessment is from
the dataset's public description page only, not from downloading and inspecting the
real `.rar` contents; per the Roofnet lesson, that confirmation step should happen
before trusting this further). Add one modern anchor point (Raca 4G/5G or WetLinks
Starlink, aggregate loss rate only, explicitly labeled "aggregate-derived, not
GE-fitted") so the parameter-space map spans both a legacy/WSN regime and a modern
cellular/LEO regime, without overstating what the aggregate-only datasets support.

## Fitter dependency decision

**Decision: hand-rolled numpy moment-method fitter, no new dependency.**
`pyproject.toml`'s numpy/scipy/matplotlib-only policy is kept. For a binary
delivery sequence, matching the marginal loss rate and the lag-1
autocorrelation (equivalently, mean good/bad run lengths) gives a closed-form
solve for `p_gb`, `p_bg` for the pure-Gilbert case (`eps_good=0`,
`eps_bad=1`, i.e. loss is a deterministic function of hidden state). This is
adequate for the parameter-space map's purpose. `hmmlearn`/Baum-Welch EM
would only be justified for the FULL Gilbert-Elliott case (`eps_good>0`,
`eps_bad<1` — loss possible in both states, hidden state not directly
observable from a single loss bit, so transition dynamics and per-state loss
rates must be jointly inferred). Kept as an optional future cross-check, not
added now.

## Reproducibility notes

- IEEE DataPort requires a free account (no payment) for CRAWDAD-collection
  downloads — reproducibility is gated by "make an account," not by cost or
  a private/ephemeral hosting arrangement.
- Roofnet's raw per-received-packet logs (not the pre-aggregated
  per-link-PRR summary some surveys cite) are the specific files needed;
  confirm which exact files are pulled when #61 implements the download step,
  since a processed/aggregated variant of the same dataset would silently
  degrade back to an aggregate-only fit.

## Estimator implementation and validation (task #61, `dmr/ge_fit.py`)

Implemented and validated against SYNTHETIC data with known ground truth
(`ge_fit_demo.py`) — this environment cannot actually create an IEEE
DataPort account and download Roofnet's ~21M-record raw traces (an access/
download step, not a math/simulation step, out of scope for this session to
do autonomously); the estimator is ready to point at real Roofnet-derived
binary sequences once #52 (or a follow-up) obtains them.

- `fit_gilbert_runlength`: exact closed-form fit (mean good/bad run
  lengths → `p_gb`, `p_bg`) for the pure-Gilbert case. Validated across 3
  parameter regimes (short-burst/rare-bad, long-burst/common-bad, moderate)
  at n=200,000 simulated steps: **max relative error 3.8%** against ground
  truth.
- `fit_gilbert_elliott_moments`: attempts the general case via marginal
  rate + lag-1/lag-2 autocovariance decay. Recovers `p_gb+p_bg` correctly
  (≤1.7% error) on the same pure-Gilbert sequences, and on a genuine full-GE
  sequence (`eps_good=0.05, eps_bad=0.6`) **correctly refuses to report**
  individual `p_gb`/`p_bg`/`eps_good`/`eps_bad` (returns `None`, asserted in
  the demo, not just claimed in prose) — a direct empirical confirmation
  that lag-1/lag-2 moments alone underdetermine the full model, exactly as
  this project's `hmmlearn`-vs-hand-rolled decision above anticipated.

## Task #52 (final phase — placement on the parameter-space map): deferred, not fabricated

#52 asks to place the fitted `(lambda, eps-contrast)` points from #61 onto
both the paper's §8.6 adaptivity-value heatmap and #56's invariant chart.
Doing this **honestly requires the actual fitted numbers**, which requires
the real Roofnet raw per-received-packet logs — and, as noted above,
obtaining those requires creating a free IEEE DataPort account and pulling
data, an external access/download step this session cannot perform
autonomously (no account credentials, no interactive signup).

An attempt was made to at least get literature-cited qualitative numbers
directly from the original Aguayo et al. (SIGCOMM 2004) Roofnet paper via
web search/fetch, to build an interim illustrative placement: search results
confirm "more than two-thirds of links deliver less than 90% (packet
delivery ratio)" and "most links have stable loss rates second-to-second,
though a small minority are bursty at that timescale" (Aguayo, D., Bicket,
J., Biswas, S., Judd, G. & Morris, R., "Link-level Measurements from an
802.11b Mesh Network", SIGCOMM 2004). Fetching the paper's PDF directly for
its actual burst-length/autocorrelation table failed — the PDF's text layer
did not extract cleanly via the available tooling.

**Decision: do not fabricate a precise `(lambda, eps-contrast)` point from
this qualitative information alone and present it as a calibrated anchor —
that would manufacture false precision on both target charts.** The
qualitative facts above are directionally usable (Roofnet is a lossy,
mostly-non-bursty-at-1s-granularity mesh — i.e., plausibly *low-to-moderate*
`lambda` relative to this project's calibrated scenario, not the
short-sharp-burst regime the counterexample lives in) but are not a
substitute for an actual per-link binary-sequence fit.

**Status update, 2026-07-19: RESOLVED for real data (not Roofnet) — the above
Roofnet-specific blocker is moot since Roofnet was replaced as the primary
candidate (see the correction earlier in this file).** The user supplied
their own authenticated IEEE DataPort session directly (browser cookies,
then a completed SAML SSO login, then a live session cookie) to unblock
this. Using that session:

1. Confirmed `CRAWDAD due/packet-delivery` (DOI 10.15783/C7NP4Z) and
   `CRAWDAD ucsb/meshnet` (DOI 10.15783/C7WS3S) both genuinely exist and are
   downloadable (the "Access Dataset" flow calls an AJAX endpoint,
   `/dataport/s3-download-url/<node-id>?key=...`, returning a short-lived
   signed S3 URL — this is the actual download mechanism on this platform).
2. Downloaded `ucsb/meshnet`'s `1144393236-1144450070.tar.gz` (1.3MB, 951
   `neighbortable-<timestamp>` files, one per minute) and confirmed by
   directly reading the extracted files that this dataset's public files
   contain **ETT (Expected Transmission Time) values per link per minute**,
   not raw per-probe records — confirming the earlier "windowed, not
   per-packet" assessment was correct.
3. Downloaded `due/packet-delivery`'s `delay_10m_12runs.rar` (24MB;
   extracted with `node-unrar-js`, a WASM-based RAR reader installed via
   `pnpm` since no system `unrar`/`7z` binary was available and `sudo`
   requires a password not available in this environment — 191MB extracted
   text file, 8064 lines/stack-parameter-configurations, each a comma-
   separated header of 7 stack parameters followed by ~3600 per-packet delay
   values for that configuration's 12 runs). **Directly confirmed the file
   contains genuine per-packet loss/success records**: the token `1111`
   appears in 47.7% of all data tokens in a spot check — far too frequent
   and exactly-repeated to be a real continuous delay measurement, confirming
   it is a fixed "packet lost" sentinel (real successful-delivery delays are
   continuously-varying values like `7.1095`, `9.1095`, `12.109`, etc.).
4. **Ran `dmr/ge_fit.py`'s `fit_gilbert_runlength` on real field data for the
   first time in this project** (`real_trace_ge_fit_demo.py`), sampling 30 of
   the 8064 stack-parameter configurations at the 10m-distance file: 21 gave
   a valid fit (9 were all-loss or all-success, undefined for a run-length
   fit). **Result: persistence `λ=1-p_gb-p_bg` ranges from -0.678 to +0.350**
   depending on protocol configuration (queue size, max retry count, TX
   power) — 5/21 configs show clearly bursty loss (`λ>0.05`, GE's classic
   assumption), 7/21 show clearly **alternating** loss (`λ<-0.05`, i.e. LESS
   memory than an i.i.d. process — a real pattern this project's GE/restless-
   bandit framing does not usually consider), and 9/21 are close to i.i.d.
   (`|λ|≤0.05`). Loss rates in the sample ranged 0.001–0.409.

**This is the first real (non-synthetic) GE calibration data point actually
obtained in this project**, and it is richer than a single anchor point: it
shows real short-range WSN loss processes span a wide range of memory
structure, not just "bursty vs. not," which is itself a useful, non-obvious
finding for framing what real channels' `(λ, contrast)` values plausibly look
like. Raw downloaded data lives in `data/ucsb_meshnet/` and
`data/due_packet_delivery/` (not committed to git — large, ~215MB combined;
re-download via IEEE DataPort with a free account if needed). Reproduce via
`uv run python real_trace_ge_fit_demo.py`.

**Still not done**: actually placing these points on the paper's §8.6
adaptivity-value heatmap and #56's invariant chart (the literal deliverable
#52 asks for) — the raw material now exists to do this properly, but the
plotting/overlay step itself was not yet performed in this session.

## Task #52 COMPLETE, 2026-07-19: real point placed on both charts

Picked two of the 21 valid real fits from `due/packet-delivery` (distance=10m)
as a hop1/hop2 pair for the belief-MDP: `HopParams(p_gb=0.066, p_bg=0.653,
eps_good=0, eps_bad=1)` (λ=+0.281, loss=0.092) and `HopParams(p_gb=0.079,
p_bg=0.915, eps_good=0, eps_bad=1)` (λ=+0.006, loss=0.079). Both are
pure-Gilbert fits (`eps_good=0, eps_bad=1` exactly) since this dataset's loss
events are a genuine binary lost/delivered outcome — contrast is trivially 1,
an honest fact about this data source, not a modeling simplification.

**Bug found and fixed first**: reusing these exact-0/1 `eps` values in
`dmr/beliefgrid2d.py`'s `bayes_update_scalar` hit a `0/0 → NaN` at
belief-simplex boundary points (`total = unnorm_bad + unnorm_good` can be
exactly 0 when `eps_good=0` or `eps_bad=1`), and `NaN * 0 = NaN` (not 0)
silently corrupted the whole RVI solve even though the erroneous branch had
zero probability mass — every prior scenario in this project used
`0 < eps < 1` strictly, so this was never triggered before. Fixed with
`np.divide(unnorm_bad, total, out=np.zeros_like(beta), where=total > 0)`.
Verified safe via regression: recomputed the paper's calibrated scenario
(hop1=(0.05,0.5,0.01,0.12), hop2=(0.02,0.05,0.01,0.6), cost_a=0.08,
c_warm=0.06, c_switch_warm=0.01, c_switch_cold=0.5, resolution=100) and got
`g*=0.074191`, an exact match to an earlier-session value computed before
this fix existed — the fix only changes behavior at the previously-broken
exact-0/1 `eps` boundary.

**§8.6 adaptivity-value sweep, real channel pair** (`real_channel_adaptivity_
sweep_demo.py`; full 8×8 grid over the same `(c_warm, c_switch_cold)` values
as the paper, `c_switch_warm=0.01` fixed): with the paper's own `cost_a=0.08`
verbatim, this real pair's path B (loses whenever *either* hop is Bad, since
`eps_bad=1` for both — a harsher, all-or-nothing structure than the paper's
graduated synthetic `eps_bad<1` scenario) has a stationary average loss of
16.4%, which is worse than `cost_a=0.08` on average everywhere, so the
optimal/always-cold policies degenerate to "always route A" for every grid
point (`g_adapt=g_cold=cost_a` exactly) — a correct but uninformative result.
**`cost_a` has no real-world counterpart in this WSN dataset regardless of
value chosen** (it is a cost-of-using-the-direct-path parameter this dataset
cannot supply), so it was raised to `0.16` (comparable to this real pair's
own 16.4% stationary loss scale, giving the relay a genuine chance to
sometimes be worth using) purely to obtain a non-degenerate comparison, not
because 0.16 is "the" calibrated value.

With `cost_a=0.16`, the sweep is non-degenerate and structured: peak relative
value **3.6% at c_warm=0.02, c_switch_cold=0.10** (vs. the paper's synthetic
scenario: 12.7% at the same `c_warm=0.02, c_switch_cold=0.20`), decaying
sharply to ~0% outside a narrow band, same qualitative shape as the paper's
Table 4 (a real but narrow sweet spot, not a broad plateau) — smaller in
magnitude for this real channel pair, but never crossing this project's
informal ">5%" "clearly worth it" bar. **Honest reading: the real channel
pair reproduces the qualitative "narrow band" shape of the paper's synthetic
finding, but with roughly 3-4x smaller peak value** — consistent with the
paper's own framing (§8.6) that whether adaptivity clears a "worth the
complexity" bar is scenario-specific, not universal.

**#56/#57 invariant placement** (`real_channel_invariant_check_demo.py`,
using `invariant_features_demo.py`'s `extract_features`, at the same real
hop pair and the peak sweep point `cost_a=0.16, c_warm=0.02,
c_switch_cold=0.10`): `contrast_product=1.0` exactly (both real hops are
pure-Gilbert), `lambda_product=+0.00169` (tiny — hop2's λ=+0.006 is nearly
memoryless), `max_voi_gap=0.0706`, giving invariant value **41.86**.
**This is far outside the #47/#56 250-scenario training distribution's
entire range** (non-violator invariant: 0–0.578; violator invariant:
0.020–0.487) — it sits at the 100th percentile of both. Root cause traced
directly: the training adversarial search's sampled `λ2` range was
`[0.051, 0.963]`, never approaching 0, so `lambda_product` near 0 (as this
real hop2 gives) was never explored, and the invariant (which divides by
`lambda_product`) blows up outside that range. **This is an honest external-
validity limitation, not a "high violation risk" finding**: #56/#57 only
established rank-based AUC separation within the searched range, never a
classification threshold, so extrapolating 70x past the training data's max
observed value is not a meaningful risk assessment either way — it is a
genuine gap (near-memoryless real hops were never covered by the adversarial
search) worth noting for future invariant-search work, not evidence the real
channel is dangerous.

Both real-data points are now rendered on `dual_mobility_relay_paper.html`
and `dual_mobility_relay_paper_en.html`'s §8.6 heatmap (a marked cell/diamond
overlay) and stated in prose in §10.5, with the out-of-range caveat spelled
out inline rather than a bare percentile number. Reproduce via
`uv run python real_channel_adaptivity_sweep_demo.py` and
`uv run python real_channel_invariant_check_demo.py`.

## Remaining open item (a): calibrating c_switch/c_warm against real QUIC timing — 2026-07-19

Per STAGE0_REPORT.md's own "conditional go" framing, the switching-cost calibration was
always the actual go/no-go tiebreaker for Stage 1 (this project's real purpose, not pure
theory). Pursued after a Codex-style consultation concluded the single-crossing conjecture
(THRESHOLD_PROOF.md, item 15) is empirically dead and no longer worth chasing, and that this
practical calibration is the highest-value remaining thread.

**Real numbers found in the PARENT isekai-terminal `rust-core` codebase** (verified directly
by grep, not taken on an agent's word):
- `HEALTH_CHECK_INTERVAL = 3s` (`isekai-transport/src/path_health.rs:52`) — the real system's
  actual route re-classification cadence. Chosen as the decision epoch `T_epoch`: the
  controller cannot re-decide routing faster than it re-scores path health. (Rejected
  alternative: the GE channel's own dwell time — that's a channel property, not a decision
  cadence, and conflating them was explicitly flagged as a mistake to avoid.)
- `OPEN_PATH_TIMEOUT = 8s` (`isekai-transport/src/multipath.rs:45`) — an empirically
  confirmed real-device (Android, cellular) FAILURE timeout for path validation (PLAN.md
  §8-4b-adjacent finding), not the duration of a successful migration. Used only as a
  pessimistic "failure corner" upper bound on `c_switch`, never presented as typical.
- No real measured duration for a *successful* PATH_CHALLENGE/PATH_RESPONSE round trip
  exists anywhere in the codebase. The optimistic end used below (~1 RTT, 0.05–0.2s) is a
  theoretical estimate, stated honestly as such, not a measured number.

**Unit bridge**: `c_switch ≈ disruption_frac × T_switch / T_epoch` (loss-per-step units,
consistent with `cost_a`/`c_switch_cold` already being expressed as per-step average-loss
fractions in STAGE0_REPORT.md), `disruption_frac∈(0,1]` (≈1 break-before-make, →0
make-before-break).

**A bigger finding surfaced while building this** (`c_switch_time_calibration_demo.py`,
Sweep B): the two real `due/packet-delivery` hop fits already used throughout #52 (native
per-packet sampling 10–30ms) were resampled to the real 3s epoch via a new closed-form
utility, `dmr/ge_fit.resample_gilbert_to_epoch` (persistence rescales as `lambda^k` for
`k=T_epoch/dt_native`, stationary P(Bad) preserved exactly — both properties of a 2-state
chain's transition-matrix eigenstructure, not an approximation). **Both real hops' lambda
collapses to exactly 0.0 in float64** at the 3s epoch (`k=100` and `k=300` native steps is
far more than enough to decay any `|lambda|<1`). Since Stage 0's own Finding 2 already
established decomposition value requires persistence (`lambda>0`) at the decision epoch,
**this real channel pair has zero decomposition value at the real system's actual re-decision
cadence, for ANY c_switch** — a more fundamental boundary than the switching-cost question
this calibration was originally built to answer. Sweep B's epoch scan (0.01s→30s) shows gain
is only non-negligible at epochs comparable to the channel's own ~10–30ms native burst
timescale — utterly impractical for a real routing controller — and is already ~0 by
`T_epoch≈0.3–1s`, well before the real 3s value.

**Sweep A** (holding channel memory fixed via a synthetic hop pair with real persistence *at*
a 3s epoch, since the real pair's own answer is exactly 0 and would make a flat, uninformative
line): gain crosses below 10% around `T_switch≈0.5s` and below 5% around `T_switch≈1–2s`
(`disruption_frac`-dependent). The optimistic successful-switch range (0.05–0.2s →
`c_switch≈0.017–0.067`) sits comfortably in the >10% "clearly worth it" band; the 8s
failure-corner (`c_switch≈2.67`) sits at the ~0.87% floor (essentially "stuck on path A"
territory, consistent with STAGE0_REPORT.md's own switching-cost-too-high failure mode).

**Honest reading**: *if* a real hop channel had persistence at the 3s cadence, a successful
switch's cost would be cheap enough to preserve most of the decomposition value — but
*repeated failed migrations* (paying the 8s timeout) would destroy it entirely. For the one
real channel pair actually calibrated so far (an indoor WSN lab benchmark, not the drone
relay's actual RF link — same caveat as always for this dataset), the channel-memory-vs-
decision-cadence mismatch dominates: it's already fully decorrelated by the time the real
system gets to act on it again, independent of `c_switch`. **The boundary that matters most
for Stage 1, at least for a channel this fast relative to a multi-second health-check
cadence, is epoch-vs-channel-timescale, not switch-cost-vs-effective-band** — a genuinely
different (and more decisive) answer than the question was originally framed around.
Reproduce via `uv run python c_switch_time_calibration_demo.py`.

**Not yet done**: placing this on the paper's charts (§8.6/§9) — deliberately deferred until
after review of the epoch-normalization step itself (the resample/normalization is precisely
the kind of dimensional-consistency step this project's established review pattern exists to
catch — see `peer-review-execute-not-just-read` precedent). Do not present these numbers in
the paper before that review completes.

## CORRECTION, 2026-07-19 (same day): the real-channel headline is RETRACTED — model artifact, not a real property

Two independent reviews of the above section (Codex CLI's usage quota was exhausted, so an
Opus-model agent substituted for it; a separate Fable-model agent reviewed in parallel,
uncoordinated). This is exactly the kind of dimensional/modeling-consistency check the gate
above existed for, and it caught a real problem — the gate worked as intended.

**What survives, confirmed correct by BOTH reviewers via independent hand-verification (matrix
power / direct recomputation), no changes needed**: `dmr/ge_fit.resample_gilbert_to_epoch`'s
closed-form eigenvalue/stationary-preservation math; the `HEALTH_CHECK_INTERVAL=3s` /
`OPEN_PATH_TIMEOUT=8s` rust-core constants and their file:line citations; the unit-bridge
formula `c_switch≈disruption_frac×T_switch/T_epoch`; Sweep A's synthetic-hop-pair crossing
points (~0.5s for 10%, ~1-2s for 5%).

**Two wording corrections** (Opus-model review, both in `c_switch_time_calibration_demo.py`'s
docstring/comments and mirrored above): (1) hop1's resampled lambda is actually `0.281^100 ≈
7.4e-56`, not "`<1e-100`" as originally written — still utterly negligible for the MDP, but the
specific magnitude claim was numerically wrong (only hop2's `0.0058^300≈1e-671` truly clears
that bar). (2) "k=100/300 is far more than enough to decay any lambda<1" is FALSE as a general
statement (`0.99^100=0.366`, not decayed) — the real reason these two hops' resampled lambda is
tiny is that their *fitted native* lambda (0.281, 0.0058) is already low, not that k is
universally large enough. This wording error actively undercut the paper's own honest caveat
about not generalizing to a different physical link (a slower-fading real drone link with
native lambda near 1 would NOT decay away at a 3s epoch) — worth fixing precisely because it
was pulling the write-up toward overclaiming generality it didn't intend to claim.

**The load-bearing retraction (Fable-model review, going further than the wording checks
above)**: the headline claim — "this real channel pair has zero decomposition value at the
real system's 3s cadence, regardless of c_switch" — does NOT survive contact with the raw
trace data it's supposedly calibrated from. Direct re-analysis of the two actual trace
sequences behind the fits (`data/due_packet_delivery/extracted/delay_10m_12runs.txt`, the exact
lines used) found:
- The empirical autocovariance decay is grossly non-geometric — a 2-state Markov chain
  predicts `cov(k+1)/cov(k)=lambda` at every lag, but hop1's ratios sit flat around 0.9-1.1
  through lag 8 (vs. predicted 0.281), and hop2's `cov(2)/cov(1)=31.8` — a ratio **impossible**
  for any 2-state chain (must lie in `[-1,1]`). This falsifies the 2-state model for at least
  hop2's real trace, not just a quantitative mismatch.
- 3-second-block loss rates vary 10-15x more than an i.i.d. binomial process would, and hop1's
  trace contains a visible real multi-second fade: **two consecutive 3s blocks at 62% and 36%
  loss** — literally the "still bad at the next decision epoch" memory the headline claims
  doesn't exist.
- Refitting `fit_gilbert_runlength` at the *correct* granularity (binarizing 3s-block loss rate,
  then run-length fitting at that block scale instead of per-packet) gives hop1
  **lambda(3s)≈+0.24 to +0.44** depending on the binarization threshold — non-trivial
  persistence at exactly the epoch the original headline said had none.

**Root cause**: `fit_gilbert_runlength` (used throughout #52's earlier real-data work, not new
today) assumes `eps_bad=1` — loss is deterministic given the hidden state. Real fades are
*partial*-loss (~30-60%, not 100%), so a real multi-second fade gets chopped by the fitter into
many short 1-2-packet "bad runs" every time a packet happens to get through, catastrophically
underestimating the true hidden-state's persistence. `resample_gilbert_to_epoch`'s `lambda^k`
rescaling then faithfully propagates this already-broken per-packet fit down to a tiny number
at the 3s scale — the resampling math itself is correct (confirmed above), it's just being fed
a wrong input. The earlier real-data section's own framing ("`eps_good=0, eps_bad=1` exactly...
an honest fact about this data source") conflated "the recorded per-packet outcome is binary"
with "the *hidden state's* emission probability is 0/1" — the former is true of this dataset,
the latter does not follow from it and is what broke here.

**Corrected status of open item (a)**: NOT resolved. The rust-core timing constants, the
resample utility, and the unit bridge are validated, reusable infrastructure. But no valid
real-channel-pair conclusion about the 3s-epoch boundary exists yet — the pure-Gilbert
per-packet fit cannot be trusted for a channel with partial-loss fades. **Next step, if
resumed**: either (a) refit directly at 3s-block granularity (bin the raw per-packet sequence
into 3s windows, define "Bad" via a loss-rate threshold per window, run `fit_gilbert_runlength`
on the resulting block-level binary sequence — Fable's finding above already prototyped this
informally and got `lambda(3s)≈0.24-0.44`, but the threshold choice needs to be principled, not
ad hoc), or (b) fit the full Gilbert-Elliott model (`eps_good>0, eps_bad<1`) via Baum-Welch/EM
at the native per-packet scale, which was previously ruled out of scope for the moment-method
estimator (`dmr/ge_fit.py`'s module docstring) but may now be necessary since the pure-Gilbert
assumption itself is what failed. Do not reuse the current `TRACE_CALIBRATION_NOTES.md` "zero
memory" numbers for anything; the corrected real-channel boundary question is still open.

## RESOLUTION (as far as available data allows), 2026-07-19, same day

Implemented (b)'s principled version directly at the 3s-block scale rather than per-packet: a
new `dmr/ge_fit.fit_ge_binomial_em` fits a 2-state HMM with **Binomial(n_t, eps_state)**
emissions per 3s window (`n_t`=packets in that window, via new `dmr/ge_fit.bin_to_windows`) using
Baum-Welch/EM (Rabiner 1989 scaled forward-backward) — no ad hoc "is this window Bad" threshold
needed, since the posterior state probabilities do that job softly, self-consistently with the
fitted `eps_good`/`eps_bad`. **Validated against synthetic ground truth first** (per this
project's own established convention): recovers `eps_good`/`eps_bad` accurately and `lambda`
within ~5% at n=500 windows across 5 seeds. Critically, also checked estimator variance at the
REAL data's actual small sample sizes (36 windows for hop1, 12 for hop2, both synthetic-ground-
truth-seeded): **at n=36, lambda estimates range from 0.15 to 0.84 across 8 seeds (true=0.80,
std=0.25); at n=12, they range from -0.29 to 0.90 (std=0.43) — essentially uninformative.** This
sets honest expectations before even looking at the real fits.

**Real fit results** (same two hop configs as throughout #52: p_gb=0.0664/p_bg=0.6526 @
30ms-native = hop1, p_gb=0.0787/p_bg=0.9155 @ 10ms-native = hop2):

- **hop1 (36 windows, 108s recording)**: EM converges to the SAME fit across all 10 different
  random initializations (`p_gb=0.168, p_bg=0.045, eps_good=0.013, eps_bad=0.132,
  lambda=+0.787`) — a stable, non-ambiguous optimum, not ricocheting between local optima the
  way the small-N synthetic stress test above worried about. The per-window loss-rate sequence
  itself shows why: a clear, visually obvious two-window fade (62%, 36% consecutive) plus a
  later sustained shift from several exact-zero-loss windows to a persistent ~5-17%-loss regime
  for the rest of the recording — real, non-i.i.d. structure a memoryless model cannot produce.
  **This directly and robustly refutes the retracted headline's "hop1 has zero persistence at
  3s" for hop1 specifically.** One honest caveat: with only one fade event and one regime shift
  in a single 108s recording, this may be capturing genuine bursty fading OR a slower
  environmental drift over the recording (e.g. distance/interference conditions changing) —
  both are real, non-negligible "the past predicts the future" structure that decomposition
  could exploit, but they are physically different phenomena worth distinguishing with more/
  longer real recordings, which this dataset (fixed 3601-packet runs, no way to get a longer
  recording from the same file) cannot supply.
- **hop2 (12 windows, 36s recording)**: EM converges to a degenerate, boundary-hugging fit
  (`p_gb=1.000, p_bg=0.222, lambda=-0.222`) — `p_gb` sitting exactly at the `[0,1]` boundary is
  a classic small-sample EM failure mode, and this specific landing point is well inside the
  synthetic small-N stress test's own noise band (`[-0.29, 0.90]` at n=12) rather than a
  trustworthy "real alternating structure" finding. **Do not use this fit for anything** — hop2's
  36-second recording is simply too short to say anything reliable about its own 3s-scale
  persistence, positive, negative, or zero.

**Corrected gain calculation** (`beliefgrid2d.belief_grid2d_value_iteration_warm`, same
`cost_a=0.16`/`c_switch_warm=0.01` as the rest of #52, `c_switch` swept across the same
real-timing-derived range as the retracted section above), pairing hop1's now-credible real 3s
fit against two hop2 scenarios since hop2's own real value is unresolved:

- **hop2 = its native per-packet fit resampled to 3s** (the "floor" — essentially memoryless,
  `lambda≈0`, per the [validated-correct, per both prior reviews] `resample_gilbert_to_epoch`):
  gain is **exactly 0.00% at every c_switch tested** (0.017 through 2.67). Even hop1's own
  substantial persistence (`lambda=0.79`) buys nothing if hop2 contributes no exploitable memory
  at all — consistent with Stage 0's own mechanism (decomposition's value comes from telling
  *which* hop is responsible for a loss so its own persistence can be exploited; a memoryless
  hop2 gives nothing to exploit even when correctly identified).
- **hop2 = a purely hypothetical "as if hop2 also had hop1's own fitted persistence"** scenario
  (NOT derived from hop2's own data — explicitly a "what if" bound, given hop2's real value is
  presently unmeasurable): gain ranges **~3.0-6.5%** across the same c_switch sweep, peaking
  around `c_switch≈0.3-0.7` — smaller than the paper's synthetic scenario's 12.7% peak, but
  clearly nonzero and would clear the informal 5% "worth it" bar for a meaningful chunk of the
  realistic c_switch range (optimistic successful-switch end: 0.017-0.067, giving 3.0-3.6%,
  below the bar; failure-adjacent end near 0.3-0.7, giving the 5-6.5% peak, above it).

**Honest final answer to "when does hop decomposition matter, when doesn't it", as far as this
one real dataset can say**: it is genuinely conditional, not a fixed yes/no, and hinges on BOTH
hops independently having real persistence at the actual decision cadence — a memoryless second
hop kills the value outright regardless of how bursty the first hop is or how cheap switching is.
Real evidence exists that at least one real short-range link (hop1 here) CAN retain non-trivial
persistence at a practical multi-second cadence (refuting the original blanket "everything
decorrelates by 3s" claim), but this specific dataset cannot supply a matching credible answer
for a second, independent hop (36 seconds of data is not enough to resolve 3s-scale persistence
either way). **Getting a real, resolved answer for a real hop PAIR would need a longer real
recording than what `due/packet-delivery`'s fixed 3601-packet runs can provide** — this is now
a data-availability limit, not a modeling-methodology gap (the EM fitter itself is validated and
this project's own numpy/scipy dependency policy is respected).

**Next steps if resumed**: (1) consolidate the block-fit + corrected-gain calculation above into
a proper `real_channel_block_fit_demo.py` script (currently only run ad hoc in-session, not yet
committed as a reproducible artifact — do this before citing these numbers anywhere durable).
(2) A third-party review of this EM implementation and its real-data application would be
prudent before any paper placement, following this project's established pattern, though the
small-sample honesty checks above (validating on synthetic data at the SAME sample sizes as the
real data, not just at a comfortable large N) were built in specifically to pre-empt the kind of
overclaim the previous round's review caught. (3) If real longer-duration traces become
available (a different dataset, or a future real Wi-Fi/UHF drone-link recording per this
project's ultimate scenario), rerun this same block-fit pipeline rather than re-deriving new
machinery.

## SECOND CORRECTION + a more fundamental data limitation, 2026-07-19, same day

`real_channel_block_fit_demo.py` was written (consolidating the above), and then a genuinely
independent replacement dataset was sought and found: CRAWDAD `isti/rural` (IEEE DataPort, DOI
10.15783/C7G01C) — 200,000 frames/trace at 5ms native sampling (~999s per trace, ~333 windows
at the 3s epoch, vs. `due/packet-delivery`'s uninformative 12-36). Downloaded via the user's own
authenticated IEEE DataPort session (same mechanism as before). See `isti_rural_block_fit_demo.py`.

**A real bug was caught while applying this to the new dataset**: `fit_ge_binomial_em`'s `seed`
parameter did NOTHING — initialization was fully deterministic (fixed `eps`/`trans`/`pi` values),
so every earlier "stability across N different random seeds" check in this project (including
the hop1 finding directly above, and `real_channel_block_fit_demo.py`'s own small-sample stress
test framing) was silently re-running the IDENTICAL computation, not testing genuine multi-start
stability. Caught because isti/rural's "5-seed check" suspiciously returned bit-identical results
every time. **Fixed** (`dmr/ge_fit.py`, `resample_gilbert_to_epoch`... no, `fit_ge_binomial_em`'s
init now genuinely randomizes via the seeded RNG) and a new `fit_ge_binomial_em_multistart`
helper added that runs N=30 genuine random starts and selects the highest-log-likelihood result
— use this, not a single arbitrary-seed call, for anything reported going forward. Also fixed a
related NaN-propagation bug (`gamma[:-1].sum(axis=0)` could be exactly 0 for one state on
real, highly-lossy data, causing 0/0→NaN that silently corrupted every subsequent EM iteration
— guarded the same way `beliefgrid2d.py`'s `bayes_update_scalar` already does elsewhere in this
project).

**Consequence: the hop1 (due/packet-delivery) finding above needs a correction.** Re-running
with genuine 30-start EM found the EM lands on TWO distinct local optima, not one: `lambda=+0.79`
(11/30 starts, loglik=-185.11) and `lambda=+0.47` (19/30 starts, loglik=-153.93 — HIGHER
log-likelihood, hence the actual best fit found). **The correct value is `lambda≈+0.47`, not
+0.79 as stated above** — the earlier claim of a single "stable, non-ambiguous optimum" was an
artifact of the seed bug, not a real finding. The qualitative conclusion (hop1 has real,
substantial persistence at 3s, refuting "zero memory") is UNCHANGED — both optima are clearly
positive and non-trivial — but do not cite the specific `+0.79` figure or the "stable across all
10 inits" framing anywhere; both are now known to be wrong. The gain-calculation numbers derived
from `hop1_real` earlier in this document (the `hop2=floor`/`hop2=hypothetical` gain sweep) used
the `+0.79` fit and would shift somewhat with the corrected `+0.47` fit — **not yet recomputed**;
treat those specific percentages as stale pending a rerun with the corrected fit.

**A more fundamental problem, found by inspecting the isti/rural fits' raw per-window loss
sequences directly (not just trusting the EM's output number)**: applying the corrected
multi-start EM to isti/rural's two picked real traces (`rcv_05M_310m_0500B.txt`, 31.6% loss;
`rcv_11M_230m_0500B.txt`, 72.5% loss) gives HIGH apparent persistence (`lambda=+0.50` and
`+0.997` respectively, both hitting a `p_gb≈0` near-boundary solution). But plotting the actual
per-window loss-rate sequence shows why: **hop1's trace opens with a brief anomalous spike
(windows 1-2 at ~0.70-0.75 loss) that decays to a stable ~0.30-0.35 baseline for the entire rest
of the 999s recording — a single one-time startup transient, not repeated bursting.** Hop2 shows
the mirror pattern: a stable ~0.78-0.84 baseline for nearly the whole recording, with a single
drop to ~0.37-0.50 in the LAST 4 windows only — a one-time end-of-recording transient (hardware
being powered down/repositioned is a plausible cause, though not confirmed). **Both hops'
"persistence" is being driven by ONE isolated transient event each, not by the channel
repeatedly switching between good/bad states during steady-state operation** — confirmed via a
model-free check (empirical per-window loss-rate autocorrelation, computed directly, no EM
involved): both traces show the classic single-transient signature (a decaying but not
oscillating autocorrelation, dominated entirely by one excursion) rather than genuine periodic-
ish bursty switching.

**Why this matters more than a wording fix**: the drone-relay routing MDP's entire premise is a
channel that REPEATEDLY transitions between good/bad states during ongoing operation (so that
"ride out THIS bad period, because you're likely back to good soon and it'll happen again" is a
meaningful, reusable policy) — not a channel that changes state ONCE over the course of a
~15-minute measurement and then stays put. A single one-time transient, however real, does not
provide evidence either way about REPEATED persistence at the 3s decision cadence, which is the
thing that actually matters for whether hop-decomposition has ongoing value. **Both real
datasets tried so far (`due/packet-delivery`'s single fade event, `isti/rural`'s single
startup/shutdown transient) share the same underlying limitation: they are STATIC point-to-point
links (fixed position, no mobility) with no ongoing physical mechanism to cause REPEATED
regime-switching.** This is not a coincidence or a dataset-picking mistake — it is a structural
consequence of testing on non-mobile links when the actual application (a physically moving
relay vehicle + drone) is inherently mobility-driven. **A dataset with genuine, ongoing mobility
(the already-identified secondary candidate, Berlin V2X — real vehicular drives through Berlin,
coarser 1s binning but actual repeated mobility-induced fading) is now the more promising
direction, not another static link.**

**Corrected status of open item (a), again**: still NOT resolved for a real hop PAIR. What IS
now solid: the resample/EM/multi-start infrastructure (bugs fixed, validated against synthetic
ground truth including the multi-optimum failure mode), the rust-core timing constants, and the
unit bridge. What is NOT yet resolved: any real evidence of REPEATED 3s-scale persistence from
an actual physically-relevant (mobile) link. Do not run or trust a decomposition-gain calculation
built on either static-link dataset's fitted "persistence" without this caveat attached — it
would be answering a question about a one-time experimental transient, not the channel dynamics
the routing model actually cares about. **Next step, if resumed**: apply the same
`bin_to_windows`/`fit_ge_binomial_em_multistart` pipeline to Berlin V2X's real vehicular sidelink
data (1s-aggregated packet error rate, ~12.75 hours across 17 real drive-rounds — far more
opportunity to see genuine REPEATED regime changes than either static link could ever offer) — a
different-shaped parsing problem (Parquet, not a raw text trace) but the same fitting machinery.

## THIRD RESOLUTION: Berlin V2X gives the first credible real-mobility answer, 2026-07-19, same day

Downloaded `sidelink_dataframe.parquet` from IEEE DataPort's Berlin V2X open-access page (real
car-to-car sidelink measurements from actual drives through Berlin) via the user's authenticated
session, using a one-off `uv run --with pandas --with pyarrow` session (NOT added to this
project's own numpy/scipy/matplotlib dependencies — same policy as the earlier one-off
`node-unrar-js` use for `.rar` extraction). The parquet already has a genuine `Packet_error_ratio`
(PER) and `Received Packets` count per 1-second row at a fixed 50Hz transmission rate, so
`total_sent=Received/(1-PER)` reconstructs an EXACT integer 50 for every row — a real per-second
Binomial(n=50, k=loss) observation, not an approximation (see `berlin_v2x_block_fit_demo.py`).

**Segmentation**: each (Source car, Destination car, Scenario) group spans MULTIPLE separate
drive-rounds with real gaps of minutes to hours between them (not one continuous recording) —
split into contiguous segments wherever consecutive `time_epoch` values jump by >3s, and used the
single longest contiguous segment per car-pair. Picked two genuinely different real links: car4→
car2 (1873s continuous, 11.1% loss) and car3→car1 (1105s continuous, 21.7% loss) — both far
longer than either static dataset's best segment (isti/rural's 999s, due/packet-delivery's 108s),
giving 624 and 368 windows at the 3s epoch respectively.

**A real data-quality wrinkle, caught and fixed**: the dataset's own `Packet_error_ratio` is
rounded to 2 decimals, so the exact-reconstruction formula occasionally lands 1-2 packets
negative (56/1873 rows in the first segment, 22/1105 in the second) — clipped to the valid
`[0,n_trials]` range in `load_per_second_arrays`, since the unclipped negative counts were
corrupting the Binomial log-likelihood (a real `invalid value encountered in subtract` warning
inside the EM, not just a cosmetic issue).

**Model-free sanity check FIRST, before trusting any EM output** (the lesson from the isti/rural
single-transient discovery above): computed each segment's own per-window loss-rate
autocorrelation AND a median-crossing count (how many times a 5-window-smoothed rate crosses its
own median — a one-off transient crosses ~0-2 times, genuine repeated switching crosses many).
**Both real vehicular segments pass this check cleanly, unlike both prior static-link
datasets**: car4→car2 shows a smoothly decaying autocorrelation (lag1=+0.49→lag20=+0.06) and **70
median-crossings over 624 windows**; car3→car1 shows the same decay shape (lag1=+0.50→lag20≈0)
and **40 median-crossings over 368 windows**. This is genuine, repeated regime-switching, not a
single startup/shutdown artifact — real evidence of the KIND of persistence the routing MDP
actually needs, from an actually-mobile link, for the first time in this project.

**EM fits are clean and non-degenerate** (30-start `fit_ge_binomial_em_multistart`, no `p_gb≈0`
boundary collapse this time): car4→car2 gives `p_gb=0.191, p_bg=0.455, eps_good=0.032,
eps_bad=0.301, lambda=+0.354`; car3→car1 gives `p_gb=0.276, p_bg=0.393, eps_good=0.070,
eps_bad=0.425, lambda=+0.330`. Both states are meaningfully separated (eps_bad roughly 6-10x
eps_good) and both hops show real, moderate (not extreme) persistence.

**First gain-sweep attempt gave a flat, uninformative 0.00% everywhere** — reusing `cost_a=0.30`
(this pair's own path-B stationary loss, ~0.304, computed from the joint independent stationary
distribution) alongside the fixed `c_warm=0.02` used throughout the rest of #52's real-data work
made `g_adapt` exactly equal `g_cold` at every `c_switch` tested: warm standby itself was simply
never worth its cost at `c_warm=0.02` for this specific pair's dynamics, so the fully-adaptive
policy degenerated to plain cold-switching (a real, correct, but uninformative result — the same
class of degenerate-parameter-choice issue this project already documented once for the WSN real
pair, just a different specific cause this time: not "path B categorically worse than cost_a",
but "warm standby categorically not worth it at this c_warm").

**The FULL 2D (c_warm, c_switch_cold) sweep** (`berlin_v2x_2d_sweep_demo.py`, matching this
project's own established `real_channel_adaptivity_sweep_demo.py` convention rather than fixing
c_warm) finds the effect IS real but small and confined to a narrow low-c_warm corner: **peak
relative value 0.65% at c_warm=0.005, c_switch_cold=0.02**, decaying to exactly 0% for any
c_warm≥0.02. No point in the swept grid clears the informal 5% "worth it" bar.

**This is the first credible, non-degenerate, real-mobility-grounded answer this project has
produced to its original applied question.** Unlike every previous real-data attempt (due/
packet-delivery's single fade event, isti/rural's single startup/shutdown transient, both real
mobility datasets' initial 0%-everywhere degenerate parameterization), this result rests on: (a)
genuinely mobile, actually-relevant links, (b) a model-free check confirming REPEATED persistence
before trusting the EM, (c) a properly-calibrated cost_a, and (d) the full 2D sweep rather than a
single c_warm slice. **Honest reading: for these two specific real vehicular sidelink traces,
hop-decomposition's value is real but small (well under 1%, an order of magnitude below the
paper's synthetic scenario's 12.7% peak and below the informal 5% threshold) — a genuine "does
not clearly matter for this pair" data point**, not a methodology failure. This is one specific
real car-pair combination, not a general statement about all vehicular V2X links — a different
real pair (different lambda combination, different loss-rate combination) could plausibly land
elsewhere on the spectrum; Berlin V2X has many more (Source,Destination,Scenario) segments not
yet tried (see the ranked list in-session: `src=3,dst=2,S2` 1756s/4.9%, `src=3,dst=1,S1` 1507s/
1.3%, etc.) if a broader real-mobility-based map is wanted later.

**Status of open item (a), final for this session**: the rust-core timing constants, the
resample/EM/multistart infrastructure (validated, bugs fixed), and now a first credible real-
mobility gain measurement all exist. The single biggest remaining gap is that ONLY ONE real
car-pair combination has been fully carried through the pipeline — broadening to several more
Berlin V2X segments (cheap: same data already downloaded, same pipeline, just different
Source/Destination/Scenario selections and re-running `berlin_v2x_2d_sweep_demo.py`) would turn
this from "one data point" into an actual map, which is what the user originally asked for.
Reproduce via `uv run python berlin_v2x_block_fit_demo.py` and
`uv run python berlin_v2x_2d_sweep_demo.py`. Raw data lives in `data/berlin_v2x/*.npz` (not
committed to git, same convention as the other real datasets) plus the source parquet under the
scratchpad directory (re-download via IEEE DataPort if needed).
