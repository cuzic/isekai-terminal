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
    println!("    connect    local stdio/TCP side (skeleton)");
    println!("    serve      remote service side (skeleton)");
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

fn helper_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISEKAI_HELPER_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let mut sibling = dir.join("isekai-helper");
            if cfg!(windows) {
                sibling.set_extension("exe");
            }
            if sibling.exists() {
                return sibling;
            }
        }
    }

    PathBuf::from("isekai-helper")
}

fn next_arg(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("isekai-pipe serve: {flag} requires a value"))
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
                let value = next_arg(&mut iter, "--service").map_err(|e| {
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
                let value = next_arg(&mut iter, "--target").map_err(|e| {
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
                let value = next_arg(&mut iter, &arg).map_err(|e| {
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

    let helper = helper_path();
    let status = match Command::new(&helper).args(&helper_args).status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!(
                "isekai-pipe serve: failed to execute legacy helper runtime at {}: {e}",
                helper.display()
            );
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };

    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::from(EX_UNAVAILABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ServeLaunch {
        parse_serve(args.iter().map(|arg| arg.to_string()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn serve_accepts_named_ssh_service() {
        let launch = parse(&[
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
        let launch = parse(&["--target", "127.0.0.1:2222"]);

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
        Some("connect") => unimplemented_command("connect"),
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
