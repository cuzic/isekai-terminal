use std::path::PathBuf;
use std::process::{Command, ExitCode};

use isekai_pipe_core::ServiceSpec;

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
    profile: String,
    service: ServiceSpec,
    ssh_args: Vec<String>,
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

fn ssh_path() -> PathBuf {
    sibling_binary_path("ISEKAI_SSH_PATH", "isekai-ssh")
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
    let mut ssh_args = Vec::new();
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!(
                    "    isekai-pipe connect --profile production --service ssh --stdio [OPTIONS]"
                );
                println!();
                println!("COMPATIBILITY:");
                println!("    Positional PROFILE is accepted as a legacy alias for --profile.");
                println!("    ISEKAI_SSH_PATH can override the legacy connect runtime path.");
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
            "--mode" | "--stun-server" | "--resume-window" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                ssh_args.push(arg);
                ssh_args.push(value);
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

    let Some(profile) = profile else {
        eprintln!("isekai-pipe connect: --profile is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    let Some(service) = service else {
        eprintln!("isekai-pipe connect: --service is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    if !stdio {
        eprintln!("isekai-pipe connect: --stdio is required until --listen is implemented");
        return Err(ExitCode::from(EX_USAGE));
    }

    Ok(Some(ConnectLaunch {
        profile,
        service,
        ssh_args,
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

fn connect_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_connect(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };

    let _service = launch.service;
    let mut ssh_args = Vec::with_capacity(2 + launch.ssh_args.len());
    ssh_args.push("connect".to_string());
    ssh_args.push(launch.profile);
    ssh_args.extend(launch.ssh_args);

    run_child("connect", ssh_path(), &ssh_args)
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

        assert_eq!(launch.profile, "production");
        assert_eq!(
            launch.service,
            ServiceSpec::new(isekai_pipe_core::ServiceName::new("ssh"), "legacy-connect").unwrap()
        );
        assert_eq!(
            launch.ssh_args,
            vec!["--mode", "relay", "--resume-window", "30"]
        );
    }

    #[test]
    fn connect_accepts_positional_profile_for_compatibility() {
        let launch = parse_connect_args(&["production", "--service", "ssh", "--stdio"]);

        assert_eq!(launch.profile, "production");
        assert!(launch.ssh_args.is_empty());
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

fn main() -> ExitCode {
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
        Some("connect") => connect_command(args),
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
