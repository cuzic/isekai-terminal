//! Loads and parses `#@isekai` directive lines out of the OpenSSH config
//! file chain (`~/.ssh/config` and whatever it `Include`s, following the
//! same `-F`-override/`Host`/`Match` resolution `ssh(1)` itself uses) into
//! a flat, ordered [`IsekaiDirective`] list. [`super::config`] is what
//! turns that list into a resolved [`super::IsekaiConfig`] — this module
//! only concerns itself with finding and reading the directive *text*.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::{ssh_option_width, WrapperPlan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IsekaiDirective {
    pub(super) name: String,
    pub(super) args: Vec<String>,
}

pub(super) fn load_isekai_directives(plan: &WrapperPlan) -> Result<Vec<IsekaiDirective>> {
    let roots = config_roots(plan);
    let mut visited = HashSet::new();
    let mut directives = Vec::new();
    for root in roots {
        if root.exists() {
            load_isekai_directives_from_file(
                &root,
                &plan.destination,
                &mut visited,
                &mut directives,
            )?;
        }
    }
    Ok(directives)
}

fn config_roots(plan: &WrapperPlan) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut i = 0;
    while i < plan.ssh_args.len() {
        match plan.ssh_args[i].as_str() {
            "-F" if i + 1 < plan.ssh_args.len() => {
                roots.push(expand_path(&plan.ssh_args[i + 1], None));
                i += 2;
            }
            "-F" => break,
            _ => i += ssh_option_width(plan.ssh_args[i].as_str()),
        }
    }
    if roots.is_empty() {
        if let Some(home) = isekai_fs_guard::resolve_home_dir() {
            roots.push(home.join(".ssh").join("config"));
        }
    }
    roots
}

fn load_isekai_directives_from_file(
    path: &Path,
    destination: &str,
    visited: &mut HashSet<PathBuf>,
    directives: &mut Vec<IsekaiDirective>,
) -> Result<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("isekai-ssh: failed to read ssh config {}", path.display()))?;
    let base_dir = path.parent();
    let mut active = true;
    let mut in_match = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#@isekai") {
            if in_match {
                return Err(anyhow!(
                    "ISEKAI_CONFIG_UNSUPPORTED_MATCH: #@isekai inside Match block"
                ));
            }
            if active {
                directives.push(parse_isekai_directive(rest.trim())?);
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        match keyword.to_ascii_lowercase().as_str() {
            "include" => {
                for pattern in split_words(rest) {
                    for include in expand_include_pattern(&pattern, base_dir)? {
                        load_isekai_directives_from_file(
                            &include,
                            destination,
                            visited,
                            directives,
                        )?;
                    }
                }
            }
            "host" => {
                in_match = false;
                active = host_patterns_match(rest, destination);
            }
            "match" => {
                in_match = true;
                active = false;
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_isekai_directive(rest: &str) -> Result<IsekaiDirective> {
    let mut words = split_words(rest);
    let name = words
        .next()
        .ok_or_else(|| anyhow!("isekai-ssh: empty #@isekai directive"))?;
    Ok(IsekaiDirective {
        name,
        args: words.collect(),
    })
}

fn split_keyword(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(index) => (&line[..index], line[index..].trim()),
        None => (line, ""),
    }
}

fn split_words(input: &str) -> impl Iterator<Item = String> + '_ {
    input.split_whitespace().map(str::to_string)
}

fn expand_include_pattern(pattern: &str, base_dir: Option<&Path>) -> Result<Vec<PathBuf>> {
    let expanded = expand_path(pattern, base_dir);
    let pattern = expanded.to_string_lossy().into_owned();
    let mut paths = Vec::new();
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        for entry in
            glob::glob(&pattern).with_context(|| format!("invalid Include pattern {pattern:?}"))?
        {
            paths.push(entry?);
        }
        paths.sort();
    } else {
        paths.push(PathBuf::from(pattern));
    }
    Ok(paths)
}

fn expand_path(input: &str, base_dir: Option<&Path>) -> PathBuf {
    let expanded = if input == "~" {
        isekai_fs_guard::resolve_home_dir().unwrap_or_else(|| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        isekai_fs_guard::resolve_home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(input))
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.unwrap_or_else(|| Path::new(".")).join(expanded)
    }
}

fn host_patterns_match(patterns: &str, destination: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        if let Some(negative) = pattern.strip_prefix('!') {
            if wildcard_match(negative, destination) {
                return false;
            }
        } else if wildcard_match(pattern, destination) {
            matched = true;
        }
    }
    matched
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], value: &[u8]) -> bool {
    match (pattern.split_first(), value.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&b'*', rest)), _) => {
            wildcard_match_bytes(rest, value)
                || value
                    .split_first()
                    .map(|(_, value_rest)| wildcard_match_bytes(pattern, value_rest))
                    .unwrap_or(false)
        }
        (Some((&b'?', rest)), Some((_, value_rest))) => wildcard_match_bytes(rest, value_rest),
        (Some((&p, rest)), Some((&v, value_rest))) if p == v => {
            wildcard_match_bytes(rest, value_rest)
        }
        _ => false,
    }
}
