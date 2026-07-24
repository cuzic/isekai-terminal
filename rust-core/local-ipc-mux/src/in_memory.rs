//! A same-process, in-memory [`ExclusiveChannel`] test double.

use std::collections::HashMap;
use std::io;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;

use crate::{ClaimError, ConnectError, ExclusiveChannel};

/// Bytes buffered per direction of an [`InMemoryChannel`] connection before
/// a write blocks — arbitrary but generous for test payloads.
const DUPLEX_BUFFER_SIZE: usize = 8192;

type Registry = Mutex<HashMap<String, mpsc::UnboundedSender<DuplexStream>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test-only [`ExclusiveChannel`] implementation backed by a process-global
/// registry (keyed by channel name) instead of any real OS IPC primitive —
/// lets a caller's owner/client logic (accept loop, framing, protocol) be
/// unit-tested without touching a real named pipe. Production code always
/// uses a real implementation (e.g. `WindowsNamedPipeChannel`).
///
/// Ownership is released (the name becomes claimable again) when this value
/// is dropped.
pub struct InMemoryChannel {
    name: String,
    incoming: mpsc::UnboundedReceiver<DuplexStream>,
}

#[async_trait]
impl ExclusiveChannel for InMemoryChannel {
    type Connection = DuplexStream;

    async fn try_claim(name: &str) -> Result<Self, ClaimError> {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if reg.contains_key(name) {
            return Err(ClaimError::AlreadyClaimed { name: name.to_string() });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        reg.insert(name.to_string(), tx);
        Ok(Self { name: name.to_string(), incoming: rx })
    }

    async fn accept(&mut self) -> io::Result<Self::Connection> {
        self.incoming.recv().await.ok_or_else(|| io::Error::other("owner was dropped"))
    }

    async fn connect(name: &str) -> Result<Self::Connection, ConnectError> {
        let tx = {
            let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
            reg.get(name).cloned().ok_or_else(|| ConnectError::NotFound { name: name.to_string() })?
        };
        let (client_side, owner_side) = tokio::io::duplex(DUPLEX_BUFFER_SIZE);
        tx.send(owner_side).map_err(|_| ConnectError::NotFound { name: name.to_string() })?;
        Ok(client_side)
    }
}

impl Drop for InMemoryChannel {
    fn drop(&mut self) {
        registry().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn second_claim_of_the_same_name_fails_while_the_first_is_alive() {
        let name = "test-channel-claim-once";
        let _owner = InMemoryChannel::try_claim(name).await.unwrap();
        match InMemoryChannel::try_claim(name).await {
            Err(ClaimError::AlreadyClaimed { .. }) => {}
            other => panic!("expected AlreadyClaimed, got a different outcome ({})", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn claiming_again_after_the_owner_drops_succeeds() {
        let name = "test-channel-reclaim-after-drop";
        let owner = InMemoryChannel::try_claim(name).await.unwrap();
        drop(owner);
        InMemoryChannel::try_claim(name).await.expect("name should be claimable again after the owner dropped");
    }

    #[tokio::test]
    async fn connect_without_an_owner_returns_not_found() {
        let err = InMemoryChannel::connect("test-channel-no-owner").await.unwrap_err();
        assert!(matches!(err, ConnectError::NotFound { .. }));
    }

    #[tokio::test]
    async fn a_connected_client_can_exchange_bytes_with_the_accepted_owner_side() {
        let name = "test-channel-roundtrip";
        let mut owner = InMemoryChannel::try_claim(name).await.unwrap();

        let mut client = InMemoryChannel::connect(name).await.unwrap();
        let mut accepted = owner.accept().await.unwrap();

        client.write_all(b"hello owner").await.unwrap();
        let mut buf = [0u8; 11];
        accepted.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello owner");

        accepted.write_all(b"hello client").await.unwrap();
        let mut buf = [0u8; 12];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello client");
    }

    #[tokio::test]
    async fn the_owner_can_accept_multiple_clients_over_its_lifetime() {
        let name = "test-channel-multi-client";
        let mut owner = InMemoryChannel::try_claim(name).await.unwrap();

        let mut client_a = InMemoryChannel::connect(name).await.unwrap();
        let mut accepted_a = owner.accept().await.unwrap();
        let mut client_b = InMemoryChannel::connect(name).await.unwrap();
        let mut accepted_b = owner.accept().await.unwrap();

        client_a.write_all(b"a").await.unwrap();
        let mut buf = [0u8; 1];
        accepted_a.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"a");

        client_b.write_all(b"b").await.unwrap();
        accepted_b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"b");
    }
}
