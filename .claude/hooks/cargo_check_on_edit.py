#!/usr/bin/env python3
"""PostToolUse hook (Write|Edit): after Claude edits a rust-core/**/*.rs file,
build just the crate that file belongs to and surface *new* compiler
diagnostics automatically.

Rewrite rationale (see .claude/rules/parallel-worktree-agent-operations.md):
the previous bash+grep version had three deterministic gaps this fixes:

1. It built the edited file's crate via that crate's *own* Cargo.toml
   (`cargo build --manifest-path <crate>/Cargo.toml`), which does not
   inherit the feature unification a real workspace build gets. A crate
   like quicmux with no `default` feature would then report every
   `#[cfg(feature = "...")]`-gated enum as empty ("non-exhaustive
   patterns") even though every real consumer requests the feature that
   makes it compile fine — a false alarm indistinguishable from a real
   regression without reading the crate's Cargo.toml by hand. This
   version resolves the *workspace* manifest above the crate and builds
   via `-p <crate> --manifest-path <workspace>/Cargo.toml`, matching how
   the crate is actually built everywhere else.
2. It reported the full warning/error text on every single edit, even
   for diagnostics already present before this edit (e.g. a whole
   platform-gated module that is *supposed* to show as dead code on
   Linux). This version caches the previous diagnostic set per worktree
   and reports only the delta.
3. It gave no indication of which worktree produced the report, which
   mattered once several agents were building in parallel worktrees —
   this version tags the report with the worktree name.

Exit 0 = silent. Exit 2 wakes Claude with stdout as feedback (see
.claude/settings.json's asyncRewake).
"""
from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path

CACHE_DIR = Path("/tmp/claude-cargo-check-cache")
MAX_OUTPUT_CHARS = 4000
BUILD_TIMEOUT_SECONDS = 280


def read_edited_file_path() -> Path | None:
    try:
        payload = json.loads(sys.stdin.read())
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    fp = (
        (payload.get("tool_input") or {}).get("file_path")
        or (payload.get("tool_response") or {}).get("filePath")
        or ""
    )
    if "/rust-core/" not in fp or not fp.endswith(".rs"):
        return None
    path = Path(fp)
    return path if path.is_absolute() else None


def repo_root(start: Path) -> Path:
    """Nearest ancestor containing .git (file or dir — works for both the
    main checkout and `git worktree add` worktrees), used to bound upward
    manifest searches so they never escape the repo."""
    d = start
    while True:
        if (d / ".git").exists():
            return d
        if d == d.parent:
            return start
        d = d.parent


def find_upwards(start: Path, filename: str, ceiling: Path) -> Path | None:
    d = start
    while True:
        candidate = d / filename
        if candidate.is_file():
            return candidate
        if d == ceiling or d == d.parent:
            return None
        d = d.parent


def crate_name_from_manifest(manifest: Path) -> str | None:
    in_package_section = False
    for line in manifest.read_text(encoding="utf-8", errors="replace").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package_section = stripped == "[package]"
            continue
        if in_package_section and stripped.startswith("name"):
            _, _, value = stripped.partition("=")
            return value.strip().strip('"')
    return None


def worktree_label(path: Path) -> str:
    parts = path.parts
    for i, part in enumerate(parts):
        if part == "worktrees" and i + 1 < len(parts):
            return parts[i + 1]
    return "main"


def diagnostic_key(message: dict) -> str:
    # File + rendered message text, deliberately *not* line number: an
    # unrelated edit earlier in the same file shifts every later
    # diagnostic's line number, which would otherwise make this hook
    # re-report already-known warnings as "new" on every subsequent edit.
    spans = message.get("spans") or []
    file_name = spans[0].get("file_name", "") if spans else ""
    text = message.get("message", "")
    return hashlib.sha256(f"{file_name}|{text}".encode()).hexdigest()


def run_cargo_build(workspace_manifest: Path, crate_name: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            "cargo", "build",
            "-p", crate_name,
            "--manifest-path", str(workspace_manifest),
            "--message-format=json",
        ],
        capture_output=True,
        text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
    )


def extract_diagnostics(stdout: str) -> dict[str, str]:
    diagnostics: dict[str, str] = {}
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if record.get("reason") != "compiler-message":
            continue
        message = record.get("message") or {}
        if message.get("level") not in ("warning", "error"):
            continue
        rendered = (message.get("rendered") or message.get("message") or "").strip()
        if rendered:
            diagnostics[diagnostic_key(message)] = rendered
    return diagnostics


def cache_file_for(workspace_dir: Path, crate_name: str) -> Path:
    digest = hashlib.sha256(str(workspace_dir).encode()).hexdigest()[:16]
    return CACHE_DIR / f"{crate_name}-{digest}.json"


def load_previous_keys(cache_file: Path) -> set[str]:
    if not cache_file.exists():
        return set()
    try:
        return set(json.loads(cache_file.read_text()))
    except (json.JSONDecodeError, OSError):
        return set()


def save_current_keys(cache_file: Path, keys: set[str]) -> None:
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    cache_file.write_text(json.dumps(sorted(keys)))


def main() -> int:
    edited_file = read_edited_file_path()
    if edited_file is None:
        return 0

    crate_manifest = find_upwards(edited_file.parent, "Cargo.toml", repo_root(edited_file))
    if crate_manifest is None:
        return 0
    crate_name = crate_name_from_manifest(crate_manifest)
    if crate_name is None:
        return 0

    # Search for a workspace manifest *above* the crate directory. Falls
    # back to the crate's own manifest when none is found — this is
    # correct as-is for the root `isekai-terminal-core` crate, whose
    # Cargo.toml already *is* rust-core's workspace manifest.
    workspace_manifest = find_upwards(
        crate_manifest.parent.parent, "Cargo.toml", repo_root(edited_file)
    ) or crate_manifest

    try:
        proc = run_cargo_build(workspace_manifest, crate_name)
    except (subprocess.TimeoutExpired, OSError):
        return 0

    diagnostics = extract_diagnostics(proc.stdout)

    if not diagnostics:
        if proc.returncode != 0 and proc.stderr.strip():
            # cargo failed before emitting any JSON diagnostics at all
            # (e.g. a manifest resolution error) — surface it rather than
            # silently swallowing a real failure.
            label = worktree_label(edited_file)
            print(f"[{label}] cargo build -p {crate_name} failed to run:\n{proc.stderr.strip()[:3000]}")
            return 2
        return 0

    cache_file = cache_file_for(workspace_manifest.parent, crate_name)
    previous_keys = load_previous_keys(cache_file)
    current_keys = set(diagnostics.keys())
    save_current_keys(cache_file, current_keys)

    new_keys = sorted(current_keys - previous_keys)
    if not new_keys:
        return 0

    label = worktree_label(edited_file)
    header = f"[{label}] cargo build -p {crate_name}: {len(new_keys)} new diagnostic(s)\n"
    body = "\n".join(diagnostics[key] for key in new_keys)
    print((header + body)[:MAX_OUTPUT_CHARS])
    return 2


if __name__ == "__main__":
    sys.exit(main())
