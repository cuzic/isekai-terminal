//! Process-level check that `isekai-ssh init --helper-manifest` actually
//! wires the CLI flags through to `isekai_release_verify::verify_artifact`
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic D). The pure verification logic itself
//! is unit-tested directly against `init::verify_helper_manifest` (faster,
//! no process spawn) — this file exists only to catch flag-parsing/wiring
//! bugs unit tests can't (e.g. a typo'd `--trusted-release-key` flag name).
//! No real `sshd`/mock server is needed: verification runs and fails
//! *before* `init` ever attempts to connect anywhere.

use std::path::PathBuf;
use std::process::Stdio as StdStdio;

use ed25519_dalek::{Signer as _, SigningKey, Verifier as _};
use isekai_release_verify::{sign_manifest, ReleaseManifest};

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn init_refuses_to_deploy_when_the_manifest_signature_is_from_an_untrusted_key() {
    let tmp = tempfile::tempdir().unwrap();
    let binary_path = tmp.path().join("fake-isekai-pipe");
    let binary_bytes = b"not-a-real-binary-but-verification-runs-before-any-ssh".to_vec();
    std::fs::write(&binary_path, &binary_bytes).unwrap();

    let attacker_key = SigningKey::from_bytes(&[3u8; 32]);
    let signed = sign_manifest(
        ReleaseManifest {
            version: "1.0.0".to_string(),
            platform: "linux".to_string(),
            architecture: "x86_64".to_string(),
            artifact_filename: "isekai-pipe".to_string(),
            size: binary_bytes.len() as u64,
            sha256: hex_sha256(&binary_bytes),
            protocol_compat: "isekai-pipe/1".to_string(),
            release_channel: "stable".to_string(),
            key_id: "prod-key".to_string(),
        },
        &attacker_key,
    );
    let manifest_path = tmp.path().join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec(&signed).unwrap()).unwrap();

    // The operator trusts a *different* key under the same key_id — the
    // real-world scenario this guards against (a distribution point serving
    // an artifact+manifest signed by a key that isn't the one the operator
    // actually trusts, e.g. a compromised signing key or a MITM).
    let trusted_key = SigningKey::from_bytes(&[4u8; 32]);
    assert_ne!(trusted_key.verifying_key(), attacker_key.verifying_key());
    // Sanity: confirm these two keys really don't cross-verify, so a false
    // pass below couldn't be an artifact of a broken test fixture.
    assert!(trusted_key.verifying_key().verify(b"anything", &attacker_key.sign(b"anything")).is_err());

    let output = tokio::process::Command::new(isekai_ssh_bin_path())
        .arg("init")
        .arg("some-untrusted-host")
        .arg("--helper-binary")
        .arg(&binary_path)
        .arg("--relay-addr")
        .arg("127.0.0.1:1")
        .arg("--relay-sni")
        .arg("relay.isekai-ssh.test")
        .arg("--relay-jwt")
        .arg("unused-jwt")
        .arg("--helper-manifest")
        .arg(&manifest_path)
        .arg("--trusted-release-key")
        .arg(format!("prod-key={}", hex_encode(trusted_key.verifying_key().to_bytes())))
        .arg("--expect-platform")
        .arg("linux")
        .arg("--expect-architecture")
        .arg("x86_64")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-ssh init");

    assert!(!output.status.success(), "init must exit non-zero when manifest verification fails");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("release manifest verification failed"), "expected a verification-failure message, got: {stderr}");
    assert!(output.stdout.is_empty(), "must not print deploy progress once verification has failed, got: {:?}", String::from_utf8_lossy(&output.stdout));
}

#[tokio::test]
async fn init_refuses_to_deploy_when_the_binary_does_not_match_the_manifest_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest_bytes = b"the-bytes-the-manifest-actually-describes".to_vec();
    let binary_path = tmp.path().join("fake-isekai-pipe");
    // Upload a *different* file than the one the manifest was signed for.
    std::fs::write(&binary_path, b"a-completely-different-swapped-binary").unwrap();

    let signing_key = SigningKey::from_bytes(&[5u8; 32]);
    let signed = sign_manifest(
        ReleaseManifest {
            version: "1.0.0".to_string(),
            platform: "linux".to_string(),
            architecture: "x86_64".to_string(),
            artifact_filename: "isekai-pipe".to_string(),
            size: manifest_bytes.len() as u64,
            sha256: hex_sha256(&manifest_bytes),
            protocol_compat: "isekai-pipe/1".to_string(),
            release_channel: "stable".to_string(),
            key_id: "prod-key".to_string(),
        },
        &signing_key,
    );
    let manifest_path = tmp.path().join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec(&signed).unwrap()).unwrap();

    let output = tokio::process::Command::new(isekai_ssh_bin_path())
        .arg("init")
        .arg("some-untrusted-host")
        .arg("--helper-binary")
        .arg(&binary_path)
        .arg("--relay-addr")
        .arg("127.0.0.1:1")
        .arg("--relay-sni")
        .arg("relay.isekai-ssh.test")
        .arg("--relay-jwt")
        .arg("unused-jwt")
        .arg("--helper-manifest")
        .arg(&manifest_path)
        .arg("--trusted-release-key")
        .arg(format!("prod-key={}", hex_encode(signing_key.verifying_key().to_bytes())))
        .arg("--expect-platform")
        .arg("linux")
        .arg("--expect-architecture")
        .arg("x86_64")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-ssh init");

    assert!(!output.status.success(), "init must exit non-zero when the uploaded binary doesn't match the manifest digest");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("release manifest verification failed"), "expected a verification-failure message, got: {stderr}");
}
