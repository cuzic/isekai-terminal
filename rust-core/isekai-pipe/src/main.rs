use std::process::ExitCode;

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
        Some("serve") => unimplemented_command("serve"),
        Some("probe") => unimplemented_command("probe"),
        Some("inspect") => unimplemented_command("inspect"),
        Some(other) => {
            eprintln!("isekai-pipe: unknown command: {other}");
            eprintln!("try `isekai-pipe --help`");
            ExitCode::from(EX_USAGE)
        }
    }
}
