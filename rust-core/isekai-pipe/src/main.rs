mod ctl;
mod engine;
mod connect;
mod inspect;
mod probe;
mod resume_loop;

use std::process::ExitCode;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use isekai_pipe_core::ServiceSpec;

pub(crate) const EX_USAGE: u8 = 64;
pub(crate) const EX_UNAVAILABLE: u8 = 69;

/// Serializes tests (across `main.rs`/`ctl.rs`) that mutate process-global
/// env vars (`ISEKAI_PIPE_PROFILES_DIR`/`ISEKAI_CTL_SOCK`). `cargo test`
/// runs `#[test]` functions on multiple threads within the same process by
/// default, and `std::env::set_var` has no thread-local scoping — without
/// this, one test's mutation can be clobbered mid-flight by a concurrently
/// running test in a different module (matches `isekai-ssh`'s
/// `HOME_ENV_LOCK` for the same reason).
#[cfg(test)]
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());
pub(crate) const DEFAULT_RESUME_WINDOW: Duration = Duration::from_secs(120);

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
    println!("    inspect    passive profile inspection (--profile, --json, --redact, --verbose)");
    println!("    ctl        title/clipboard control-plane client (see `isekai-pipe ctl --help`)");
    println!("    version    print version");
    println!();
    println!(
        "The command surface is reserved for the staged isekai-helper -> isekai-pipe migration."
    );
}

#[derive(Debug)]
struct ServeLaunch {
    service: ServiceSpec,
    helper_args: Vec<String>,
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
                println!("    Existing helper protocol clients are still supported.");
                return Ok(None);
            }
            "--service" => {
                let value = connect::next_arg("serve", &mut iter, "--service").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = ServiceSpec::parse(&value).map_err(|e| {
                    eprintln!("isekai-pipe serve: invalid --service {value:?}: {e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if spec.name().as_str() != "ssh" {
                    eprintln!(
                        "isekai-pipe serve: only ssh service is wired to the helper runtime for now"
                    );
                    return Err(ExitCode::from(EX_USAGE));
                }
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe serve: only one --service is supported for now");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--target" => {
                let value = connect::next_arg("serve", &mut iter, "--target").map_err(|e| {
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
            | "--bootstrap-request-file"
            | "--log-level" => {
                let value = connect::next_arg("serve", &mut iter, &arg).map_err(|e| {
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

/// The `-R` remote path convention `isekai-ssh`'s `ctl_forward.rs` uses
/// (`/tmp/isekai-pipe-ctl-<128bit hex>.sock`, opt-in `#@isekai ctl-socket
/// yes`, `ISEKAI_PIPE_DESIGN.md` §8 Epic M). `sshd` owns cleaning up the
/// actual streamlocal forward bind on a normal disconnect; this sweep only
/// catches what's left behind by abnormal exits (crash, `kill -9`, a
/// network drop that skipped `ssh -O cancel -R`).
const CTL_SOCKET_REMOTE_PREFIX: &str = "isekai-pipe-ctl-";
const CTL_SOCKET_STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Best-effort, non-fatal: a sweep failure (e.g. `/tmp` unreadable for
/// some reason) should never block `serve` from starting.
fn sweep_stale_ctl_sockets_on_remote() {
    match isekai_pipe_core::sweep_stale_sockets(
        std::path::Path::new("/tmp"),
        CTL_SOCKET_REMOTE_PREFIX,
        CTL_SOCKET_STALE_THRESHOLD,
    ) {
        Ok(removed) if !removed.is_empty() => {
            log::info!("isekai-pipe serve: swept {} stale ctl-socket file(s) under /tmp", removed.len());
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("isekai-pipe serve: failed to sweep stale ctl-socket files under /tmp: {e}");
        }
    }
}

async fn serve_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_serve(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };

    sweep_stale_ctl_sockets_on_remote();

    let mut helper_args = launch.helper_args;
    helper_args.push("--service-name".to_string());
    helper_args.push(launch.service.name().as_str().to_string());
    helper_args.push("--target".to_string());
    helper_args.push(launch.service.target().to_string());

    match engine::run_from_args(helper_args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_serve_args(args: &[&str]) -> ServeLaunch {
        parse_serve(args.iter().map(|arg| arg.to_string()))
            .unwrap()
            .unwrap()
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

/// Larger than the OS-default *process* main thread stack, which is fixed
/// at link time and cannot be grown at runtime -- notably 1 MiB on Windows
/// (vs. ~8 MiB typical on Linux/macOS). `tokio::main`'s generated
/// `block_on` runs the top-level future's synchronous work (e.g. the QUIC
/// handshake in `isekai-transport`'s `quicmux`/`noq` backend, which is deep
/// and un-inlined in debug builds) directly on the calling thread, so a
/// small main-thread stack overflows there before this fix -- observed in
/// practice as `thread 'main' has overflowed its stack` right after
/// `quicmux::noq_backend` starts connecting, when `isekai-pipe connect` runs
/// as an `isekai-ssh`-launched `ssh(1)` `ProxyCommand` on a debug build on
/// Windows. Running the whole async body on a freshly spawned thread with
/// an explicit stack size sidesteps the platform-fixed main-thread limit;
/// the size chosen here is generous headroom rather than a tight bound.
const MAIN_WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    std::thread::Builder::new()
        .name("isekai-pipe-main".to_string())
        .stack_size(MAIN_WORKER_STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(run())
        })
        .expect("failed to spawn isekai-pipe main worker thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

async fn run() -> ExitCode {
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
        Some("connect") => connect::connect_command(args).await,
        Some("serve") => serve_command(args).await,
        Some("probe") => probe::probe_command(args).await,
        Some("inspect") => inspect::inspect_command(args).await,
        Some("ctl") => ctl::ctl_command(args).await,
        Some(other) => {
            eprintln!("isekai-pipe: unknown command: {other}");
            eprintln!("try `isekai-pipe --help`");
            ExitCode::from(EX_USAGE)
        }
    }
}
