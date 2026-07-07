use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use base64::Engine as _;
use isekai_pipe_core::{
    claim_connection_intent, default_runtime_dir, BootstrapProvenance, ConnectionIntent,
    IntentTransport, ServerIdentity, ServiceSpec,
};
use isekai_transport::{
    connect_stun_p2p, connect_via_relay, ByteStream, RelayTarget, StunP2pTarget,
    SystemQuicEndpointFactory,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const EX_USAGE: u8 = 64;
const EX_UNAVAILABLE: u8 = 69;

fn print_help() {
    println!("isekai-pipe - data plane for isekai-ssh");
    println!();
    println!("USAGE:");
    println!("    isekai-pipe <COMMAND> [OPTIONS]");
    println!();
    println!("COMMANDS:");
    println!("    connect    local stdio side");
    println!("    serve      remote service side");
    println!("    probe      connectivity probe (skeleton)");
    println!("    inspect    profile/path inspection (skeleton)");
    println!("    version    print version");
    println!();
    println!(
        "The command surface is reserved for the staged isekai-helper -> isekai-pipe migration."
    );
}

fn unimplemented_command(command: &str) -> ExitCode {
    eprintln!("isekai-pipe {command}: not implemented yet");
    ExitCode::from(EX_USAGE)
}

#[derive(Debug)]
struct ServeLaunch {
    service: ServiceSpec,
    helper_args: Vec<String>,
}

#[derive(Debug)]
struct ConnectLaunch {
    profile: Option<String>,
    service: Option<ServiceSpec>,
    stdio: bool,
    mode: ConnectMode,
    stun_server: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectMode {
    Relay,
    Stun,
}

fn sibling_binary_path(env_var: &str, name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(env_var) {
        return PathBuf::from(path);
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let mut sibling = dir.join(name);
            if cfg!(windows) {
                sibling.set_extension("exe");
            }
            if sibling.exists() {
                return sibling;
            }
        }
    }

    PathBuf::from(name)
}

fn helper_path() -> PathBuf {
    sibling_binary_path("ISEKAI_HELPER_PATH", "isekai-helper")
}

fn next_arg(
    command: &str,
    iter: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("isekai-pipe {command}: {flag} requires a value"))
}

fn validate_connect_service(value: &str) -> Result<ServiceSpec, ExitCode> {
    let spec = ServiceSpec::new(isekai_pipe_core::ServiceName::new(value), "legacy-connect")
        .map_err(|e| {
            eprintln!("isekai-pipe connect: invalid --service {value:?}: {e}");
            ExitCode::from(EX_USAGE)
        })?;
    if spec.name().as_str() != "ssh" {
        eprintln!(
            "isekai-pipe connect: only ssh service is wired to the legacy connect runtime for now"
        );
        return Err(ExitCode::from(EX_USAGE));
    }
    Ok(spec)
}

fn parse_connect(args: impl Iterator<Item = String>) -> Result<Option<ConnectLaunch>, ExitCode> {
    let mut profile: Option<String> = None;
    let mut service: Option<ServiceSpec> = None;
    let mut stdio = false;
    let mut mode = ConnectMode::Relay;
    let mut stun_server = None;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!(
                    "    isekai-pipe connect --profile production --service ssh --stdio [OPTIONS]"
                );
                println!();
                println!("INTENT:");
                println!("    If ISEKAI_INTENT_ID is set, the matching ConnectionIntent is claimed first.");
                return Ok(None);
            }
            "--profile" => {
                let value = next_arg("connect", &mut iter, "--profile").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if profile.replace(value).is_some() {
                    eprintln!("isekai-pipe connect: only one --profile is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--service" => {
                let value = next_arg("connect", &mut iter, "--service").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = validate_connect_service(&value)?;
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe connect: only one --service is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--stdio" => stdio = true,
            "--mode" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                mode = match value.as_str() {
                    "relay" => ConnectMode::Relay,
                    "stun" => ConnectMode::Stun,
                    _ => {
                        eprintln!("isekai-pipe connect: --mode must be relay or stun");
                        return Err(ExitCode::from(EX_USAGE));
                    }
                };
            }
            "--stun-server" => {
                stun_server = Some(next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            "--resume-window" => {
                let _ = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                eprintln!("isekai-pipe connect: --resume-window is reserved until pipe-owned resume lands");
            }
            "--listen" => {
                eprintln!(
                    "isekai-pipe connect: --listen is not wired to the legacy connect runtime yet"
                );
                return Err(ExitCode::from(EX_USAGE));
            }
            other if other.starts_with('-') => {
                eprintln!("isekai-pipe connect: unsupported option: {other}");
                return Err(ExitCode::from(EX_USAGE));
            }
            positional => {
                if profile.replace(positional.to_string()).is_some() {
                    eprintln!("isekai-pipe connect: multiple profiles were provided");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
        }
    }

    if !stdio {
        eprintln!("isekai-pipe connect: --stdio is required until --listen is implemented");
        return Err(ExitCode::from(EX_USAGE));
    }
    if std::env::var_os("ISEKAI_INTENT_ID").is_none() {
        if profile.is_none() {
            eprintln!(
                "isekai-pipe connect: --profile is required when ISEKAI_INTENT_ID is not set"
            );
            return Err(ExitCode::from(EX_USAGE));
        }
        if service.is_none() {
            eprintln!(
                "isekai-pipe connect: --service is required when ISEKAI_INTENT_ID is not set"
            );
            return Err(ExitCode::from(EX_USAGE));
        }
    }

    Ok(Some(ConnectLaunch {
        profile,
        service,
        stdio,
        mode,
        stun_server,
    }))
}

fn run_child(command_name: &str, binary: PathBuf, args: &[String]) -> ExitCode {
    let status = match Command::new(&binary).args(args).status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!(
                "isekai-pipe {command_name}: failed to execute legacy runtime at {}: {e}",
                binary.display()
            );
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };

    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::from(EX_UNAVAILABLE),
    }
}

async fn connect_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_connect(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };

    match run_connect(launch).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

async fn run_connect(launch: ConnectLaunch) -> Result<()> {
    let intent = resolve_connection_intent(&launch)?;
    if intent.service != "ssh" {
        anyhow::bail!(
            "isekai-pipe connect: unsupported service in ConnectionIntent: {}",
            intent.service
        );
    }

    let stream: Box<dyn ByteStream> = match intent.transport {
        IntentTransport::Relay {
            helper_addr,
            server_name,
            session_secret_b64,
        } => {
            let factory = SystemQuicEndpointFactory;
            connect_via_relay(
                &factory,
                &RelayTarget {
                    helper_addr: helper_addr
                        .parse()
                        .with_context(|| format!("invalid relay helper_addr {helper_addr:?}"))?,
                    server_name,
                    cert_sha256_hex: intent.expected_server_identity.cert_sha256_hex,
                    session_secret: decode_secret(&session_secret_b64)?,
                },
            )
            .await
            .context("isekai-pipe connect: relay transport failed")?
        }
        IntentTransport::StunP2p {
            stun_server,
            peer_addr,
            server_name,
            session_secret_b64,
        } => connect_stun_p2p(
            stun_server
                .parse()
                .with_context(|| format!("invalid stun_server {stun_server:?}"))?,
            &StunP2pTarget {
                peer_addr: peer_addr
                    .parse()
                    .with_context(|| format!("invalid stun peer_addr {peer_addr:?}"))?,
                server_name,
                cert_sha256_hex: intent.expected_server_identity.cert_sha256_hex,
                session_secret: decode_secret(&session_secret_b64)?,
            },
        )
        .await
        .map(|conn| conn.stream)
        .context("isekai-pipe connect: STUN P2P transport failed")?,
    };

    relay_stdio(stream).await
}

fn resolve_connection_intent(launch: &ConnectLaunch) -> Result<ConnectionIntent> {
    if let Some(intent_id) = std::env::var_os("ISEKAI_INTENT_ID") {
        let intent_id = intent_id.to_string_lossy();
        let runtime_dir = default_runtime_dir()
            .context("isekai-pipe connect: could not determine runtime dir")?;
        return claim_connection_intent(&runtime_dir, &intent_id)
            .context("isekai-pipe connect: failed to claim ConnectionIntent");
    }

    let profile = launch.profile.as_deref().context("missing profile")?;
    let service = launch
        .service
        .as_ref()
        .map(|service| service.name().as_str())
        .unwrap_or("ssh");
    intent_from_profile(profile, service, launch)
}

fn intent_from_profile(
    profile: &str,
    service: &str,
    launch: &ConnectLaunch,
) -> Result<ConnectionIntent> {
    let key = isekai_trust::normalize_host_port(profile)
        .with_context(|| format!("isekai-pipe connect: invalid profile {profile:?}"))?;
    let store_path = isekai_trust::default_trust_store_path()
        .context("isekai-pipe connect: could not determine trust store path")?;
    let store = isekai_trust::load_trust_store(&store_path).with_context(|| {
        format!(
            "isekai-pipe connect: failed to load {}",
            store_path.display()
        )
    })?;
    let entry = store.get(&key).with_context(|| {
        format!(
            "isekai-pipe connect: profile {profile:?} is not trusted yet (looked up as {key:?})"
        )
    })?;

    let transport = match launch.mode {
        ConnectMode::Relay => IntentTransport::Relay {
            helper_addr: entry.cached_relay_addr.clone(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: entry.cached_session_secret.clone(),
        },
        ConnectMode::Stun => IntentTransport::StunP2p {
            stun_server: launch
                .stun_server
                .clone()
                .context("isekai-pipe connect: --stun-server is required with --mode stun")?,
            peer_addr: entry.cached_relay_addr.clone(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: entry.cached_session_secret.clone(),
        },
    };

    Ok(ConnectionIntent::new(
        profile,
        service,
        ServerIdentity {
            cert_sha256_hex: entry.cached_cert_sha256.clone(),
        },
        transport,
        BootstrapProvenance::TrustStore { key },
    ))
}

fn decode_secret(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("invalid session_secret_b64")
}

async fn relay_stdio(stream: Box<dyn ByteStream>) -> Result<()> {
    let (mut quic_read, mut quic_write) = stream.split();
    let mut c2h = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = stdin.read(&mut buf).await.context("reading stdin failed")?;
            if n == 0 {
                let _ = quic_write.shutdown().await;
                return Ok::<_, anyhow::Error>(());
            }
            quic_write
                .write_all(&buf[..n])
                .await
                .context("writing to remote stream failed")?;
        }
    });
    let mut h2c = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = quic_read
                .read(&mut buf)
                .await
                .context("reading remote stream failed")?;
            if n == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            stdout
                .write_all(&buf[..n])
                .await
                .context("writing stdout failed")?;
            stdout.flush().await.context("flushing stdout failed")?;
        }
    });

    let (mut c2h_done, mut h2c_done) = (false, false);
    while !c2h_done || !h2c_done {
        tokio::select! {
            res = &mut c2h, if !c2h_done => {
                c2h_done = true;
                res.context("stdin->remote task panicked")??;
            }
            res = &mut h2c, if !h2c_done => {
                h2c_done = true;
                res.context("remote->stdout task panicked")??;
            }
        }
    }
    Ok(())
}

fn parse_serve(args: impl Iterator<Item = String>) -> Result<Option<ServeLaunch>, ExitCode> {
    let mut service: Option<ServiceSpec> = None;
    let mut helper_args = Vec::new();
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!("    isekai-pipe serve --service ssh=127.0.0.1:22 [HELPER_OPTIONS]");
                println!();
                println!("COMPATIBILITY:");
                println!("    --target 127.0.0.1:22 is accepted as --service ssh=127.0.0.1:22");
                println!("    ISEKAI_HELPER_PATH can override the helper binary path.");
                return Ok(None);
            }
            "--service" => {
                let value = next_arg("serve", &mut iter, "--service").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = ServiceSpec::parse(&value).map_err(|e| {
                    eprintln!("isekai-pipe serve: invalid --service {value:?}: {e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if spec.name().as_str() != "ssh" {
                    eprintln!(
                        "isekai-pipe serve: only ssh service is wired to the legacy helper runtime for now"
                    );
                    return Err(ExitCode::from(EX_USAGE));
                }
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe serve: only one --service is supported for now");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--target" => {
                let value = next_arg("serve", &mut iter, "--target").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = ServiceSpec::ssh_target(value).map_err(|e| {
                    eprintln!("isekai-pipe serve: invalid --target: {e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe serve: --target conflicts with --service");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--once" => helper_args.push(arg),
            "--bind"
            | "--idle-timeout"
            | "--resume-window"
            | "--resume-buffer-size"
            | "--max-idle-lifetime"
            | "--max-sessions"
            | "--stun-server"
            | "--punch-peer"
            | "--relay"
            | "--relay-sni"
            | "--relay-jwt"
            | "--relay-jwt-file"
            | "--log-level" => {
                let value = next_arg("serve", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                helper_args.push(arg);
                helper_args.push(value);
            }
            other => {
                eprintln!("isekai-pipe serve: unsupported option: {other}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }

    let Some(service) = service else {
        eprintln!("isekai-pipe serve: at least one --service is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(ServeLaunch {
        service,
        helper_args,
    }))
}

fn serve_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_serve(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };

    let mut helper_args = launch.helper_args;
    helper_args.push("--target".to_string());
    helper_args.push(launch.service.target().to_string());

    run_child("serve", helper_path(), &helper_args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_connect_args(args: &[&str]) -> ConnectLaunch {
        parse_connect(args.iter().map(|arg| arg.to_string()))
            .unwrap()
            .unwrap()
    }

    fn parse_serve_args(args: &[&str]) -> ServeLaunch {
        parse_serve(args.iter().map(|arg| arg.to_string()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn connect_accepts_profile_service_and_stdio() {
        let launch = parse_connect_args(&[
            "--profile",
            "production",
            "--service",
            "ssh",
            "--stdio",
            "--mode",
            "relay",
            "--resume-window",
            "30",
        ]);

        assert_eq!(launch.profile.as_deref(), Some("production"));
        assert_eq!(
            launch.service,
            Some(
                ServiceSpec::new(isekai_pipe_core::ServiceName::new("ssh"), "legacy-connect")
                    .unwrap()
            )
        );
        assert!(launch.stdio);
        assert_eq!(launch.mode, ConnectMode::Relay);
    }

    #[test]
    fn connect_accepts_positional_profile_for_compatibility() {
        let launch = parse_connect_args(&["production", "--service", "ssh", "--stdio"]);

        assert_eq!(launch.profile.as_deref(), Some("production"));
        assert!(launch.stdio);
    }

    #[test]
    fn connect_rejects_non_ssh_service_until_dispatch_exists() {
        assert!(parse_connect(
            [
                "--profile",
                "production",
                "--service",
                "postgres",
                "--stdio"
            ]
            .into_iter()
            .map(str::to_string)
        )
        .is_err());
    }

    #[test]
    fn connect_requires_stdio_until_listen_exists() {
        assert!(parse_connect(
            ["--profile", "production", "--service", "ssh"]
                .into_iter()
                .map(str::to_string)
        )
        .is_err());
    }

    #[test]
    fn serve_accepts_named_ssh_service() {
        let launch = parse_serve_args(&[
            "--service",
            "ssh=127.0.0.1:22",
            "--bind",
            "127.0.0.1:0",
            "--once",
        ]);

        assert_eq!(
            launch.service,
            ServiceSpec::ssh_target("127.0.0.1:22").unwrap()
        );
        assert_eq!(launch.helper_args, vec!["--bind", "127.0.0.1:0", "--once"]);
    }

    #[test]
    fn serve_maps_legacy_target_to_ssh_service() {
        let launch = parse_serve_args(&["--target", "127.0.0.1:2222"]);

        assert_eq!(
            launch.service,
            ServiceSpec::ssh_target("127.0.0.1:2222").unwrap()
        );
        assert!(launch.helper_args.is_empty());
    }

    #[test]
    fn serve_rejects_unknown_services_until_dispatch_exists() {
        assert!(parse_serve(
            ["--service", "postgres=127.0.0.1:5432"]
                .into_iter()
                .map(str::to_string)
        )
        .is_err());
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("-h") | Some("--help") | Some("help") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("version") | Some("--version") => {
            println!("isekai-pipe {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("connect") => connect_command(args).await,
        Some("serve") => serve_command(args),
        Some("probe") => unimplemented_command("probe"),
        Some("inspect") => unimplemented_command("inspect"),
        Some(other) => {
            eprintln!("isekai-pipe: unknown command: {other}");
            eprintln!("try `isekai-pipe --help`");
            ExitCode::from(EX_USAGE)
        }
    }
}
