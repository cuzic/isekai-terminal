# local-ipc-mux

Exclusive single-owner local IPC channel: of several sibling processes on the
same machine that all want to reach "the one shared resource for this
session", exactly one becomes the *owner* (it claimed the channel name
first) and the rest connect to it as *clients*. A generic length-prefixed
framing/relay layer sits on top. This crate has no opinion on what bytes
flow over an established connection or what the shared resource actually is.

## Status

Early skeleton: the [`ExclusiveChannel`] trait, error types, and an
in-process [`InMemoryChannel`] test double are implemented and tested. The
real `WindowsNamedPipeChannel` implementation and the `framing` module are
still placeholders — see their module docs for the intended design.

## Platform support

Windows only for now. A Unix implementation (e.g. bind-exclusive semantics
over a `UnixListener`) is deliberately out of scope: real `ssh(1)`'s own
`ControlMaster`/`ControlPersist` already gives Unix this exact capability for
free. The `ExclusiveChannel` trait boundary is designed so a Unix
implementation can be added later without disturbing any existing caller.

## Example

```rust,ignore
use local_ipc_mux::{ExclusiveChannel, InMemoryChannel};

// Whichever sibling process gets here first becomes the owner...
match InMemoryChannel::try_claim("my-channel").await {
    Ok(mut owner) => loop {
        let conn = owner.accept().await?;
        tokio::spawn(handle_client(conn));
    },
    Err(local_ipc_mux::ClaimError::AlreadyClaimed { .. }) => {
        // ...and every later sibling connects to it as a client instead.
        let conn = InMemoryChannel::connect("my-channel").await?;
        handle_as_client(conn).await;
    }
    Err(e) => return Err(e.into()),
}
```
