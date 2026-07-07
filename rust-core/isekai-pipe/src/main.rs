use std::process::ExitCode;

use isekai_pipe_core::ServiceSpec;

const EX_USAGE: u8 = 64;

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

fn serve_command(args: impl Iterator<Item = String>) -> ExitCode {
    let mut services = Vec::new();
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!("    isekai-pipe serve --service ssh=127.0.0.1:22 [OPTIONS]");
                println!();
                println!("COMPATIBILITY:");
                println!("    --target 127.0.0.1:22 is accepted as --service ssh=127.0.0.1:22");
                return ExitCode::SUCCESS;
            }
            "--service" => {
                let Some(value) = iter.next() else {
                    eprintln!("isekai-pipe serve: --service requires name=target");
                    return ExitCode::from(EX_USAGE);
                };
                match ServiceSpec::parse(&value) {
                    Ok(spec) => services.push(spec),
                    Err(e) => {
                        eprintln!("isekai-pipe serve: invalid --service {value:?}: {e}");
                        return ExitCode::from(EX_USAGE);
                    }
                }
            }
            "--target" => {
                let Some(value) = iter.next() else {
                    eprintln!("isekai-pipe serve: --target requires ADDR:PORT");
                    return ExitCode::from(EX_USAGE);
                };
                match ServiceSpec::ssh_target(value) {
                    Ok(spec) => services.push(spec),
                    Err(e) => {
                        eprintln!("isekai-pipe serve: invalid --target: {e}");
                        return ExitCode::from(EX_USAGE);
                    }
                }
            }
            other => {
                eprintln!("isekai-pipe serve: unsupported option in skeleton: {other}");
                return ExitCode::from(EX_USAGE);
            }
        }
    }

    if services.is_empty() {
        eprintln!("isekai-pipe serve: at least one --service is required");
        return ExitCode::from(EX_USAGE);
    }

    for service in &services {
        eprintln!(
            "isekai-pipe serve: accepted service {}={} (serve runtime not wired yet)",
            service.name().as_str(),
            service.target()
        );
    }
    ExitCode::from(EX_USAGE)
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
