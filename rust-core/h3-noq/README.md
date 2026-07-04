# h3-noq

QUIC transport implementation for [`h3`](https://github.com/hyperium/h3) backed by
[`noq`](https://crates.io/crates/noq) (a fork of `quinn` adding multipath QUIC support),
instead of `quinn` itself (as `h3-quinn` does) or `msquic` (as `h3-util`'s `msquic-async`
feature does).

## Why

Written to let a client speak the same HTTP/3 + MASQUE (`CONNECT-UDP`, RFC 9298) wire
protocol as `seera-networks/ISEKAI-link`'s relay/`agent` (which use `msquic` via
`channel-masque`/`h3-util`) without depending on `msquic` at all. `msquic` has no official
Android support and an experimental-only Rust binding, which makes it a poor fit for
`isekai-terminal` (an Android app) even though it's a reasonable choice for a Linux relay
server. Since `channel-masque`'s own stack is already QUIC-backend-agnostic (`h3-util`
supports quinn/msquic/msquic-async/s2n-quic/quiche behind feature flags, and is tested
against `quinn` in its own dev-dependencies), and the wire protocol (HTTP/3 + MASQUE) is
standardized, a `noq`-backed client can interoperate with the `msquic`-backed relay/agent
by construction — same `h3`/`h3-datagram` crate driving the protocol logic on both ends,
only the low-level QUIC socket binding differs.

## Status

Ported from `h3-quinn` (same `hyperium/h3` PR #340 revision this crate depends on).
`noq` is API-compatible with `quinn` for everything `h3-quinn` touches, with two
differences accounted for here:

- `noq::RecvStream::read_chunk(max_length)` takes one fewer argument (no `ordered` bool,
  always ordered) and already returns `Option<Bytes>` instead of `Option<Chunk>`.
- `noq::ReadError` has no `IllegalOrderedRead` variant.

Everything else (`Connection`, `SendStream`, `VarInt`, `ConnectionError`, `ReadError`,
`WriteError`, `SendDatagramError`, `StreamId` conversions) matches `quinn` exactly.

Validated with two end-to-end integration tests (`tests/smoke.rs`) that drive a real
noq QUIC connection (self-signed cert via `rcgen`, loopback) through `h3-noq`:

- `h3_over_noq_get_request_round_trips`: a full HTTP/3 GET request/response.
- `h3_datagram_over_noq_round_trips` (needs `--features datagram`): an HTTP/3 Datagram
  (RFC 9297) round-trips in both directions on a request stream — the actual prerequisite
  MASQUE `CONNECT-UDP` needs.

```
cargo test --features datagram
```

## Caveats

- `h3` and `h3-datagram` are pinned to a specific unmerged upstream PR revision
  (`hyperium/h3` PR #340, since `h3-datagram` isn't in a released `h3` version yet), not a
  crates.io release. `seera-networks/ISEKAI-link` depends on the same PR revision (via a
  `masa-koz/tonic-h3` fork for `h3-util`), so this isn't a new category of risk relative to
  what that project already accepts in production — but it does mean tracking upstream by
  hand (rebasing `rev` as needed) rather than `cargo update`.
- Not yet used for anything beyond this smoke test — no real MASQUE `CONNECT-UDP` client/
  server has been built on top of it yet.
