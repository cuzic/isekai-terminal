//! `isekai-ssh build-profile add|list|remove|test`
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic P): manages `build_profile.rs`'s
//! `~/.config/isekai-ssh/build_profiles.toml`. This is deliberately the only
//! surface for authoring a build profile — no GUI, no `#@isekai` directive —
//! per the design discussion recorded in Epic P: a config GUI was judged
//! disproportionate to "edit a handful of TOML entries", and connection
//! config (`#@isekai`) and local automation config (this) are different
//! concerns that don't belong in the same file.

use anyhow::{Context, Result};

use crate::build_profile::{self, BuildProfile, BuildProfileStore};
use crate::cli::{BuildProfileAddArgs, BuildProfileCommand, BuildProfileListArgs, BuildProfileRemoveArgs, BuildProfileTestArgs};

pub async fn run(command: BuildProfileCommand) -> Result<()> {
    match command {
        BuildProfileCommand::Add(args) => add(args),
        BuildProfileCommand::List(args) => list(args),
        BuildProfileCommand::Remove(args) => remove(args),
        BuildProfileCommand::Test(args) => test(args).await,
    }
}

fn load() -> Result<(std::path::PathBuf, BuildProfileStore)> {
    let path = build_profile::default_build_profiles_path()?;
    let store = build_profile::load_build_profiles(&path)?;
    Ok((path, store))
}

fn add(args: BuildProfileAddArgs) -> Result<()> {
    let (path, mut store) = load()?;
    build_profile::upsert_profile(
        &mut store,
        BuildProfile {
            host: args.host.clone(),
            name: args.name.clone(),
            dir: args.dir,
            command: args.command,
            result_glob: args.result_glob,
            dest_dir: args.dest_dir,
        },
    )?;
    build_profile::save_build_profiles(&path, &store)?;
    println!("isekai-ssh: registered build profile {:?}/{:?}", args.host, args.name);
    Ok(())
}

fn list(args: BuildProfileListArgs) -> Result<()> {
    let (_, store) = load()?;
    let mut matched = 0;
    for profile in &store.profiles {
        if let Some(host) = &args.host {
            if &profile.host != host {
                continue;
            }
        }
        matched += 1;
        println!("{}/{}", profile.host, profile.name);
        println!("  dir:     {}", profile.dir);
        println!("  command: {}", profile.command);
        if let (Some(result_glob), Some(dest_dir)) = (&profile.result_glob, &profile.dest_dir) {
            println!("  result:  {result_glob} -> {dest_dir}");
        }
    }
    if matched == 0 {
        println!("isekai-ssh: no build profiles registered{}", args.host.map(|h| format!(" for {h:?}")).unwrap_or_default());
    }
    Ok(())
}

fn remove(args: BuildProfileRemoveArgs) -> Result<()> {
    let (path, mut store) = load()?;
    if !build_profile::remove_profile(&mut store, &args.host, &args.name) {
        anyhow::bail!("isekai-ssh: no build profile registered for {:?}/{:?}", args.host, args.name);
    }
    build_profile::save_build_profiles(&path, &store)?;
    println!("isekai-ssh: removed build profile {:?}/{:?}", args.host, args.name);
    Ok(())
}

/// Runs the profile's command locally right now, bypassing the ctl-socket
/// entirely — lets a human validate `dir`/`command` before ever wiring up a
/// remote invocation, using this process's own inherited stdout/stderr
/// rather than the chunked/streamed replay `ctl_forward.rs` does for a real
/// remote-triggered build.
async fn test(args: BuildProfileTestArgs) -> Result<()> {
    let (_, store) = load()?;
    let profile = build_profile::find_profile(&store, &args.host, &args.name)
        .with_context(|| format!("isekai-ssh: no build profile registered for {:?}/{:?}", args.host, args.name))?
        .clone();

    println!("isekai-ssh: running {:?} in {:?}", profile.command, profile.dir);
    let status = crate::build_exec::spawn_shell_command(&profile.command, &profile.dir)
        .status()
        .await
        .with_context(|| format!("isekai-ssh: failed to run build profile {:?}/{:?}", args.host, args.name))?;
    println!("isekai-ssh: exited with {status}");
    if let (Some(result_glob), Some(dest_dir)) = (&profile.result_glob, &profile.dest_dir) {
        let matches = crate::build_exec::glob_results(&profile.dir, result_glob)?;
        println!("isekai-ssh: result_glob {result_glob:?} matched {} file(s) (would push to {dest_dir:?}):", matches.len());
        for m in &matches {
            println!("  {}", m.display());
        }
    }
    if !status.success() {
        anyhow::bail!("isekai-ssh: build profile {:?}/{:?} exited non-zero", args.host, args.name);
    }
    Ok(())
}
