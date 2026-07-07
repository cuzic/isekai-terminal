//! Minimal OpenSSH frontend for the `chatgpt.md` migration path.
//!
//! Existing subcommands (`connect`, `init`, `login`, `logout`) remain the
//! compatibility surface. A non-subcommand invocation, such as
//! `isekai-ssh production`, is treated as an OpenSSH invocation with an
//! injected `ProxyCommand` that delegates the byte stream to `isekai-pipe
//! connect`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Result};
use isekai_pipe_core::{
    default_runtime_dir, write_connection_intent, BootstrapProvenance, ConnectionIntent,
    IntentTransport, ServerIdentity,
};
use tokio::process::Command;

const LEGACY_SUBCOMMANDS: &[&str] = &["connect", "init", "login", "logout"];

#[derive(Debug, PartialEq, Eq)]
struct WrapperPlan {
    openssh_path: PathBuf,
    pipe_path: PathBuf,
    profile: String,
    ssh_args: Vec<String>,
}

pub fn should_run_wrapper(args: &[String]) -> bool {
    let Some(first) = args.first().map(String::as_str) else {
        return false;
    };
    !matches!(first, "-h" | "--help" | "help" | "-V" | "--version")
        && !LEGACY_SUBCOMMANDS.contains(&first)
}

pub async fn run(args: Vec<String>) -> Result<u8> {
    let plan = parse_wrapper(args)?;
    let intent = build_connection_intent(&plan)?;
    let runtime_dir = default_runtime_dir()?;
    write_connection_intent(&runtime_dir, &intent)?;
    let proxy_command = proxy_command(&plan.pipe_path, &plan.profile);

    let mut command = Command::new(&plan.openssh_path);
    command
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", &runtime_dir)
        .arg("-o")
        .arg(format!("ProxyCommand={proxy_command}"))
        .args(&plan.ssh_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command.status().await.map_err(|e| {
        anyhow!(
            "isekai-ssh: failed to execute OpenSSH at {}: {e}",
            plan.openssh_path.display()
        )
    })?;
    Ok(status.code().unwrap_or(1) as u8)
}

fn build_connection_intent(plan: &WrapperPlan) -> Result<ConnectionIntent> {
    let key = isekai_trust::normalize_host_port(&plan.profile)
        .map_err(|e| anyhow!("isekai-ssh: invalid destination {:?}: {e}", plan.profile))?;
    let store_path = isekai_trust::default_trust_store_path()?;
    let store = isekai_trust::load_trust_store(&store_path)?;
    let entry = store.get(&key).ok_or_else(|| {
        anyhow!(
            "isekai-ssh: {:?} is not a trusted host yet (looked up as {:?} in {})",
            plan.profile,
            key,
            store_path.display()
        )
    })?;

    Ok(ConnectionIntent::new(
        plan.profile.clone(),
        "ssh",
        ServerIdentity {
            cert_sha256_hex: entry.cached_cert_sha256.clone(),
        },
        IntentTransport::Relay {
            helper_addr: entry.cached_relay_addr.clone(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: entry.cached_session_secret.clone(),
        },
        BootstrapProvenance::TrustStore { key },
    ))
}

fn parse_wrapper(args: Vec<String>) -> Result<WrapperPlan> {
    let mut openssh_path = PathBuf::from("ssh");
    let mut pipe_path = default_pipe_path();
    let mut ssh_args = Vec::new();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--isekai-ssh-path" => {
                openssh_path = PathBuf::from(next_value(&mut iter, "--isekai-ssh-path")?);
            }
            "--isekai-pipe-path" => {
                pipe_path = PathBuf::from(next_value(&mut iter, "--isekai-pipe-path")?);
            }
            _ => ssh_args.push(arg),
        }
    }

    let profile = find_destination(&ssh_args)
        .ok_or_else(|| anyhow!("isekai-ssh: destination is required"))?
        .to_string();

    Ok(WrapperPlan {
        openssh_path,
        pipe_path,
        profile,
        ssh_args,
    })
}

fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    iter.next()
        .ok_or_else(|| anyhow!("isekai-ssh: {flag} requires a value"))
}

fn default_pipe_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISEKAI_PIPE_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let mut sibling = dir.join("isekai-pipe");
            if cfg!(windows) {
                sibling.set_extension("exe");
            }
            if sibling.exists() {
                return sibling;
            }
        }
    }

    PathBuf::from("isekai-pipe")
}

fn proxy_command(pipe_path: &Path, profile: &str) -> String {
    format!(
        "{} connect --profile {} --service ssh --stdio",
        shell_quote(&pipe_path.display().to_string()),
        shell_quote(profile)
    )
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn find_destination(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return args.get(i + 1).map(String::as_str);
        }
        if !arg.starts_with('-') || arg == "-" {
            return Some(arg);
        }
        i += ssh_option_width(arg);
    }
    None
}

fn ssh_option_width(arg: &str) -> usize {
    if matches!(
        arg,
        "-B" | "-b"
            | "-c"
            | "-D"
            | "-E"
            | "-e"
            | "-F"
            | "-I"
            | "-i"
            | "-J"
            | "-L"
            | "-l"
            | "-m"
            | "-O"
            | "-o"
            | "-p"
            | "-Q"
            | "-R"
            | "-S"
            | "-W"
            | "-w"
    ) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn wrapper_is_only_for_non_subcommand_invocations() {
        assert!(!should_run_wrapper(&s(&[])));
        assert!(!should_run_wrapper(&s(&["connect", "host"])));
        assert!(!should_run_wrapper(&s(&["--help"])));
        assert!(should_run_wrapper(&s(&["production"])));
    }

    #[test]
    fn parses_wrapper_options_and_preserves_ssh_args() {
        let plan = parse_wrapper(s(&[
            "--isekai-ssh-path",
            "/usr/bin/ssh",
            "--isekai-pipe-path",
            "/opt/isekai pipe",
            "-p",
            "2222",
            "user@production",
            "uptime",
        ]))
        .unwrap();

        assert_eq!(plan.openssh_path, PathBuf::from("/usr/bin/ssh"));
        assert_eq!(plan.pipe_path, PathBuf::from("/opt/isekai pipe"));
        assert_eq!(plan.profile, "user@production");
        assert_eq!(
            plan.ssh_args,
            s(&["-p", "2222", "user@production", "uptime"])
        );
    }

    #[test]
    fn finds_destination_after_common_ssh_options() {
        assert_eq!(
            find_destination(&s(&[
                "-i",
                "id key",
                "-o",
                "StrictHostKeyChecking=no",
                "prod"
            ])),
            Some("prod")
        );
        assert_eq!(find_destination(&s(&["-vvv", "--", "prod"])), Some("prod"));
    }

    #[test]
    fn proxy_command_quotes_path_and_profile_for_shell() {
        assert_eq!(
            proxy_command(Path::new("/opt/isekai pipe"), "prod'host"),
            "'/opt/isekai pipe' connect --profile 'prod'\\''host' --service ssh --stdio"
        );
    }

    #[test]
    fn builds_connection_intent_from_trust_store() {
        let home =
            std::env::temp_dir().join(format!("isekai-ssh-wrapper-intent-{}", std::process::id()));
        let config = home.join(".config").join("isekai-ssh");
        std::fs::create_dir_all(&config).unwrap();
        let trust = r#"
[helpers."production:22"]
identity_pubkey = "pk"
trusted_helper_sha256 = "sha"
trusted_helper_version = "0.1.0"
update_policy = "exact-digest-only"
trusted_at = "2026-07-04T00:00:00Z"
last_seen_at = "2026-07-04T00:00:00Z"
cached_relay_addr = "127.0.0.1:1234"
cached_cert_sha256 = "ab"
cached_session_secret = "c2VjcmV0"
"#;
        std::fs::write(config.join("known_helpers.toml"), trust).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        let plan = WrapperPlan {
            openssh_path: PathBuf::from("ssh"),
            pipe_path: PathBuf::from("isekai-pipe"),
            profile: "production".to_string(),
            ssh_args: s(&["production"]),
        };
        let intent = build_connection_intent(&plan).unwrap();

        assert_eq!(intent.profile, "production");
        assert_eq!(intent.service, "ssh");
        assert_eq!(
            intent.transport,
            IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string()
            }
        );

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(home);
    }
}
