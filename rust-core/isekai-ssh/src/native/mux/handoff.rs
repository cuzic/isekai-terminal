//! Passphrase hand-off for the `ControlPersist`-equivalent detached holder
//! (Phase 1b): a detached holder (`super::holder`) has no console, so it can
//! never itself prompt for a passphrase-protected identity's passphrase —
//! `run_authenticated_session` forces `silent = true` in holder mode
//! specifically because of this. So when the *spawning* client (still
//! interactive) discovers it needs to spawn a holder, it decrypts every
//! passphrase-protected candidate identity **once**, upfront, right here, and
//! hands the cleartext PEM bytes to the holder over the holder's own stdin
//! pipe (never argv/env — see [`super::holder::HolderSpawner::spawn`]'s
//! `handoff` parameter) so the holder can authenticate without ever needing
//! to ask.
//!
//! The resolved set is also reused by the spawning client itself if the
//! holder still fails to authenticate and this process falls back to a
//! direct connect (`super::dispatch`) — so a user is never prompted for the
//! same passphrase twice in one invocation.
//!
//! **Wire format** (`encode`/`decode`): a flat, length-prefixed list of
//! `(candidate_path, private_key_pem, Option<certificate_pem>)` entries. Not
//! meant to be a stable cross-version protocol — the spawning process and the
//! holder it just spawned are always the exact same binary — so there is no
//! version field, matching `super::protocol`'s own reasoning for *not*
//! needing forward/backward compatibility across a self-re-exec boundary
//! (unlike the mux frame protocol, which *can* cross a version skew if an
//! owner and client somehow came from different builds).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use russh_keys::PrivateKey;
use zeroize::Zeroizing;

use crate::log_file::log_line;
use crate::native::private_key;

/// One already-decrypted identity, keyed by its on-disk candidate path (see
/// [`HandoffCredentials`]) so `connect_and_authenticate`'s candidate loop can
/// look it up by the same path `identity_file_candidates` produces.
pub(crate) struct HandoffCredential {
    pub(crate) private_key_pem: Zeroizing<Vec<u8>>,
    pub(crate) certificate_pem: Option<Vec<u8>>,
}

impl Clone for HandoffCredential {
    fn clone(&self) -> Self {
        Self { private_key_pem: self.private_key_pem.clone(), certificate_pem: self.certificate_pem.clone() }
    }
}

/// A path-keyed set of already-decrypted identities, built once by
/// [`resolve_handoff_credentials`] and consulted by
/// `connect_and_authenticate` — skipping the on-disk
/// `SessionError::EncryptedPrivateKey`/prompt path entirely for any candidate
/// it covers, since the whole point is that the holder process consulting it
/// can't prompt. `Clone`able because the same in-memory set is used twice in
/// one invocation: once handed off (encoded) to the spawned holder, and again
/// by the spawning client's own fallback direct connect if the holder still
/// fails (see this module's docs).
#[derive(Default, Clone)]
pub(crate) struct HandoffCredentials(HashMap<PathBuf, HandoffCredential>);

impl HandoffCredentials {
    pub(crate) fn get(&self, path: &Path) -> Option<&HandoffCredential> {
        self.0.get(path)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Decrypts every passphrase-protected candidate identity up front — the
/// *whole* set, unlike `connect_and_authenticate`'s ordinary lazy one-at-a-
/// time on-disk handling — because a holder can never prompt again once
/// detached, so every candidate that might need a passphrase has to be
/// resolved now. A candidate that isn't encrypted, can't be read, is
/// unparseable, or whose passphrase prompt is refused/exhausts its retries is
/// simply absent from the returned set; `connect_and_authenticate` falls
/// through to its normal on-disk handling for it (which, for a holder, means
/// it just won't be usable there — same as today, no regression).
pub(crate) fn resolve_handoff_credentials(
    host_config: &openssh_config::HostConfig,
    home: &Path,
    prompt_passphrase: &(dyn Fn(&Path, u32) -> Option<String> + Send + Sync),
) -> HandoffCredentials {
    let mut resolved = HashMap::new();
    let candidates = private_key::identity_file_candidates(&host_config.identity_file, home);
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(private_key_pem) = private_key::read_key_bytes(candidate) else { continue };
        match PrivateKey::from_openssh(&private_key_pem) {
            Ok(key) if !key.is_encrypted() => continue, // no hand-off needed; the holder can read+auth this itself
            Ok(_) => {}                                 // encrypted; fall through to the passphrase retry loop below
            Err(_) => continue,                         // unparseable — the ordinary path will hit (and skip) this too
        }

        let certificate_pem = private_key::resolve_certificate_file(host_config, candidate, index).and_then(|p| std::fs::read(&p).ok());

        let mut decrypted_pem = None;
        for attempt in 1..=3 {
            let Some(passphrase) = prompt_passphrase(candidate, attempt) else { break };
            let Ok(parsed) = PrivateKey::from_openssh(&private_key_pem) else { break };
            match parsed.decrypt(&passphrase) {
                Ok(cleartext) => match cleartext.to_openssh(Default::default()) {
                    Ok(pem) => {
                        decrypted_pem = Some(Zeroizing::new(pem.as_bytes().to_vec()));
                        break;
                    }
                    Err(e) => {
                        log_line!("isekai-ssh: failed to re-serialize the decrypted key for {}: {e}", candidate.display());
                        break;
                    }
                },
                Err(_) => continue, // wrong passphrase: retry with a fresh prompt
            }
        }
        let Some(private_key_pem) = decrypted_pem else {
            log_line!(
                "isekai-ssh: could not unlock {} for the detached mux holder hand-off; it will be unusable there \
                 (the holder itself never prompts)",
                candidate.display()
            );
            continue;
        };
        resolved.insert(candidate.clone(), HandoffCredential { private_key_pem, certificate_pem });
    }
    HandoffCredentials(resolved)
}

/// Serializes `credentials` to the flat byte format [`decode`] reads back —
/// see this module's docs on why there's no version field. Zeroizing because
/// the buffer holds cleartext private key material end-to-end until the
/// holder consumes it.
pub(crate) fn encode(credentials: &HandoffCredentials) -> Zeroizing<Vec<u8>> {
    let mut buf = Vec::new();
    write_u32(&mut buf, credentials.0.len() as u32);
    for (path, credential) in &credentials.0 {
        write_bytes(&mut buf, path.as_os_str().as_encoded_bytes());
        write_bytes(&mut buf, &credential.private_key_pem);
        match &credential.certificate_pem {
            Some(cert) => {
                buf.push(1);
                write_bytes(&mut buf, cert);
            }
            None => buf.push(0),
        }
    }
    Zeroizing::new(buf)
}

/// Reads back the format [`encode`] writes. `Ok(HandoffCredentials::default())`
/// for an empty input (the common case: no hand-off was needed, so the
/// holder's stdin — piped only when [`encode`]'s output was actually
/// written, see [`super::holder::HolderSpawner::spawn`] — is immediately
/// EOF). Any parse error (truncated/malformed input) is the caller's to
/// decide how to treat; it always means "no usable hand-off", never a panic.
pub(crate) fn decode(bytes: &[u8]) -> Result<HandoffCredentials> {
    if bytes.is_empty() {
        return Ok(HandoffCredentials::default());
    }
    let mut cursor = bytes;
    let count = read_u32(&mut cursor)?;
    let mut resolved = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let path_bytes = read_bytes(&mut cursor)?;
        let path = PathBuf::from(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(path_bytes.to_vec()) });
        let private_key_pem = Zeroizing::new(read_bytes(&mut cursor)?.to_vec());
        let has_cert = *cursor.first().ok_or_else(|| anyhow!("isekai-ssh: truncated handoff payload (missing certificate flag)"))?;
        cursor = &cursor[1..];
        let certificate_pem = match has_cert {
            0 => None,
            1 => Some(read_bytes(&mut cursor)?.to_vec()),
            other => return Err(anyhow!("isekai-ssh: malformed handoff payload (bad certificate flag {other})")),
        };
        resolved.insert(path, HandoffCredential { private_key_pem, certificate_pem });
    }
    Ok(HandoffCredentials(resolved))
}

fn write_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_be_bytes());
}

fn write_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    write_u32(buf, bytes.len() as u32);
    buf.extend_from_slice(bytes);
}

fn read_u32(cursor: &mut &[u8]) -> Result<u32> {
    if cursor.len() < 4 {
        return Err(anyhow!("isekai-ssh: truncated handoff payload (expected a length prefix)"));
    }
    let (head, rest) = cursor.split_at(4);
    *cursor = rest;
    Ok(u32::from_be_bytes(head.try_into().expect("split_at(4) guarantees exactly 4 bytes")))
}

fn read_bytes<'a>(cursor: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = read_u32(cursor)? as usize;
    if cursor.len() < len {
        return Err(anyhow!("isekai-ssh: truncated handoff payload (expected {len} more bytes, got {})", cursor.len()));
    }
    let (head, rest) = cursor.split_at(len);
    *cursor = rest;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_passphrase_prompt(_path: &Path, _attempt: u32) -> Option<String> {
        None
    }

    #[test]
    fn resolve_handoff_credentials_skips_a_missing_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![dir.path().join("does-not-exist")], ..Default::default() };
        let resolved = resolve_handoff_credentials(&host_config, dir.path(), &no_passphrase_prompt);
        assert!(resolved.is_empty(), "a missing candidate must yield an empty hand-off set, never a panic");
    }

    #[test]
    fn resolve_handoff_credentials_skips_an_unencrypted_key_without_prompting() {
        use rand::rngs::OsRng;
        use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("id_ed25519");
        let key = SshPrivateKey::from(Ed25519Keypair::random(&mut OsRng));
        std::fs::write(&key_path, key.to_openssh(Default::default()).unwrap().as_bytes()).unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![key_path], ..Default::default() };

        let prompted = std::sync::atomic::AtomicBool::new(false);
        let resolved = resolve_handoff_credentials(&host_config, dir.path(), &|path, attempt| {
            prompted.store(true, std::sync::atomic::Ordering::SeqCst);
            no_passphrase_prompt(path, attempt)
        });

        assert!(resolved.is_empty(), "an unencrypted key needs no hand-off — the holder can read it itself");
        assert!(!prompted.load(std::sync::atomic::Ordering::SeqCst), "an unencrypted key must never trigger a passphrase prompt");
    }

    #[test]
    fn resolve_handoff_credentials_decrypts_an_encrypted_key_with_the_right_passphrase() {
        use rand::rngs::OsRng;
        use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("id_ed25519");
        let key = SshPrivateKey::from(Ed25519Keypair::random(&mut OsRng));
        let encrypted = key.encrypt(&mut OsRng, "hunter2").unwrap();
        std::fs::write(&key_path, encrypted.to_openssh(Default::default()).unwrap().as_bytes()).unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![key_path.clone()], ..Default::default() };

        let resolved = resolve_handoff_credentials(&host_config, dir.path(), &|_path, _attempt| Some("hunter2".to_string()));

        let credential = resolved.get(&key_path).expect("the encrypted key must be resolved into the hand-off set");
        let cleartext = PrivateKey::from_openssh(&credential.private_key_pem).expect("the hand-off PEM must be valid OpenSSH text");
        assert!(!cleartext.is_encrypted(), "the hand-off PEM must be the decrypted cleartext key, not the original ciphertext");
    }

    #[test]
    fn resolve_handoff_credentials_gives_up_after_three_wrong_passphrases() {
        use rand::rngs::OsRng;
        use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("id_ed25519");
        let key = SshPrivateKey::from(Ed25519Keypair::random(&mut OsRng));
        let encrypted = key.encrypt(&mut OsRng, "hunter2").unwrap();
        std::fs::write(&key_path, encrypted.to_openssh(Default::default()).unwrap().as_bytes()).unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![key_path.clone()], ..Default::default() };

        let attempts = std::sync::atomic::AtomicU32::new(0);
        let resolved = resolve_handoff_credentials(&host_config, dir.path(), &|_path, _attempt| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some("wrong-passphrase".to_string())
        });

        assert!(resolved.is_empty(), "an unrecoverable key must simply be absent from the hand-off set");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3, "must try exactly 3 passphrases, matching try_encrypted_identity's own retry count");
    }

    #[test]
    fn resolve_handoff_credentials_stops_immediately_when_the_prompt_is_refused() {
        use rand::rngs::OsRng;
        use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("id_ed25519");
        let key = SshPrivateKey::from(Ed25519Keypair::random(&mut OsRng));
        let encrypted = key.encrypt(&mut OsRng, "hunter2").unwrap();
        std::fs::write(&key_path, encrypted.to_openssh(Default::default()).unwrap().as_bytes()).unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![key_path], ..Default::default() };

        let resolved = resolve_handoff_credentials(&host_config, dir.path(), &no_passphrase_prompt);
        assert!(resolved.is_empty(), "a refused prompt (silent mode) must yield an empty hand-off set, not hang or panic");
    }

    #[test]
    fn encode_then_decode_round_trips_an_empty_set() {
        let empty = HandoffCredentials::default();
        let decoded = decode(&encode(&empty)).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_of_a_genuinely_empty_byte_slice_is_also_an_empty_set() {
        // The common case: no hand-off was ever written (holder's stdin was
        // null, or piped-but-nothing-written), so the holder reads zero bytes.
        let decoded = decode(&[]).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn encode_then_decode_round_trips_a_credential_without_a_certificate() {
        let mut resolved = HashMap::new();
        resolved.insert(
            PathBuf::from("/home/u/.ssh/id_ed25519"),
            HandoffCredential { private_key_pem: Zeroizing::new(b"fake cleartext key bytes".to_vec()), certificate_pem: None },
        );
        let credentials = HandoffCredentials(resolved);

        let decoded = decode(&encode(&credentials)).unwrap();
        let got = decoded.get(Path::new("/home/u/.ssh/id_ed25519")).unwrap();
        assert_eq!(*got.private_key_pem, b"fake cleartext key bytes");
        assert!(got.certificate_pem.is_none());
    }

    #[test]
    fn encode_then_decode_round_trips_a_credential_with_a_certificate() {
        let mut resolved = HashMap::new();
        resolved.insert(
            PathBuf::from("/home/u/.ssh/id_ed25519"),
            HandoffCredential {
                private_key_pem: Zeroizing::new(b"fake cleartext key bytes".to_vec()),
                certificate_pem: Some(b"fake cert bytes".to_vec()),
            },
        );
        let credentials = HandoffCredentials(resolved);

        let decoded = decode(&encode(&credentials)).unwrap();
        let got = decoded.get(Path::new("/home/u/.ssh/id_ed25519")).unwrap();
        assert_eq!(*got.private_key_pem, b"fake cleartext key bytes");
        assert_eq!(got.certificate_pem.as_deref(), Some(&b"fake cert bytes"[..]));
    }

    #[test]
    fn encode_then_decode_round_trips_multiple_credentials() {
        let mut resolved = HashMap::new();
        resolved.insert(
            PathBuf::from("/a/id_ed25519"),
            HandoffCredential { private_key_pem: Zeroizing::new(b"key-a".to_vec()), certificate_pem: None },
        );
        resolved.insert(
            PathBuf::from("/b/id_rsa"),
            HandoffCredential { private_key_pem: Zeroizing::new(b"key-b".to_vec()), certificate_pem: Some(b"cert-b".to_vec()) },
        );
        let credentials = HandoffCredentials(resolved);

        let decoded = decode(&encode(&credentials)).unwrap();
        assert_eq!(*decoded.get(Path::new("/a/id_ed25519")).unwrap().private_key_pem, b"key-a");
        assert_eq!(*decoded.get(Path::new("/b/id_rsa")).unwrap().private_key_pem, b"key-b");
        assert_eq!(decoded.get(Path::new("/b/id_rsa")).unwrap().certificate_pem.as_deref(), Some(&b"cert-b"[..]));
    }

    #[test]
    fn decode_rejects_a_truncated_payload_instead_of_panicking() {
        let mut resolved = HashMap::new();
        resolved.insert(
            PathBuf::from("/a/id_ed25519"),
            HandoffCredential { private_key_pem: Zeroizing::new(b"key-a".to_vec()), certificate_pem: None },
        );
        let encoded = encode(&HandoffCredentials(resolved));
        let truncated = &encoded[..encoded.len() - 3];
        assert!(decode(truncated).is_err(), "a truncated payload must be a clean Err, never a panic");
    }
}
