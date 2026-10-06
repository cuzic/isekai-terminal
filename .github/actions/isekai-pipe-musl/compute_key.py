#!/usr/bin/env python3
"""isekai-pipe muslバイナリのキャッシュキーを計算する(`action.yml`参照)。

キーは「isekai-pipeの成果物を決めうる入力」だけのハッシュにする:
  - isekai-pipe自身と、そこから(dev-dependency以外で)推移的に到達できる
    workspace内pathクレートのディレクトリ配下の全trackedファイル
    (`git ls-files -s`のblob SHA。テストファイル等も含む過大近似で、取りこぼしより安全側)
  - rust-core/Cargo.lock・rust-core/Cargo.toml・rust-core/.cargo/config.toml・
    build-isekai-pipe-musl.sh・このaction自身
  - `rustc -vV`(ランナーイメージのRust更新で自動的に無効化)と zig のバージョン

依存クロージャは`cargo metadata --no-deps`のpath依存から毎回動的に求めるので、
isekai-pipeの依存クレートが増減してもこのファイルの手修正は不要。
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys


def run(cmd, cwd=None):
    return subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True).stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--zig-version", required=True)
    ap.add_argument("--repo-root", default=".")
    args = ap.parse_args()

    root = os.path.abspath(args.repo_root)
    rust_core = os.path.join(root, "rust-core")
    meta = json.loads(
        run(
            [
                "cargo",
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
                os.path.join(rust_core, "Cargo.toml"),
            ]
        )
    )
    by_dir = {}
    by_name = {}
    for pkg in meta["packages"]:
        d = os.path.dirname(pkg["manifest_path"])
        by_dir[d] = pkg
        by_name[pkg["name"]] = pkg

    closure = set()
    stack = [os.path.dirname(by_name["isekai-pipe"]["manifest_path"])]
    while stack:
        d = stack.pop()
        if d in closure:
            continue
        closure.add(d)
        for dep in by_dir[d]["dependencies"]:
            # dev-dependencyは成果物(リリースバイナリ)に影響しない。
            # normal(None)/buildはいずれも含める。
            if dep.get("path") and dep.get("kind") != "dev":
                stack.append(dep["path"])

    rel_dirs = sorted(os.path.relpath(d, root) for d in closure)
    h = hashlib.sha256()

    def feed(label, data):
        h.update(label.encode() + b"\0" + data.encode() + b"\0")

    feed("closure", "\n".join(rel_dirs))
    feed("ls-files", run(["git", "ls-files", "-s", "--", *rel_dirs], cwd=root))
    extra = [
        "rust-core/Cargo.lock",
        "rust-core/Cargo.toml",
        "rust-core/.cargo/config.toml",
        "rust-core/scripts/build-isekai-pipe-musl.sh",
        ".github/actions/isekai-pipe-musl/action.yml",
        ".github/actions/isekai-pipe-musl/compute_key.py",
    ]
    feed("extra", run(["git", "ls-files", "-s", "--error-unmatch", "--", *extra], cwd=root))
    feed("rustc", run(["rustc", "-vV"]))
    feed("zig", args.zig_version)

    digest = h.hexdigest()[:40]
    print("crates: " + " ".join(rel_dirs), file=sys.stderr)
    print(f"key=isekai-pipe-musl-v1-{digest}")


if __name__ == "__main__":
    main()
