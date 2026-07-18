# openssh-config

Resolves a deliberate subset of OpenSSH `ssh_config(5)` keywords —
`HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump`/`ForwardAgent`/
`IdentityAgent` — for a given destination host, following the same
`Host`/`Include` structural semantics `ssh(1)` itself uses:

- First obtained value wins per key (an earlier, more specific `Host` block
  beats a later `Host *`), except `IdentityFile` which accumulates across
  every matching block, in file order — matching real `ssh_config(5)`.
- `Include` splices the referenced file's lines in at that point; glob
  patterns expand in sorted order; a file that's already been visited
  (cyclic includes) is silently skipped on repeat.
- `Host` patterns support `*`/`?` wildcards and `!negation`, same as
  `ssh_config(5)`.

## What this crate deliberately does not do

- **`Match` block conditions are not evaluated.** A `Match exec`/`Match
  host`/`Match user`/... line is recognized structurally (so it doesn't get
  misparsed as a keyword), but everything inside a `Match` block is simply
  never applied. This crate has no opinion on process execution or the
  runtime context those conditions need.
- **No other keywords are parsed.** `ProxyCommand`, `CertificateFile`,
  `IdentitiesOnly`, cipher/kex algorithm lists, etc. are silently ignored.
  This is not a general-purpose `ssh_config(5)` parser — just the keywords
  listed above.
- `ProxyJump`'s value is returned as the raw string (e.g.
  `"user@jump-host:2222"` or a comma-separated multi-hop chain) — parsing it
  into individual hops is the caller's job.

## Example

```rust,no_run
let config = openssh_config::resolve_default("example-host")?;
println!("{:?} {:?} {:?}", config.host_name, config.user, config.port);
# Ok::<(), openssh_config::Error>(())
```
