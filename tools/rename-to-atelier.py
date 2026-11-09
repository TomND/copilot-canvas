#!/usr/bin/env python3
"""Renames Copilot Canvas to Atelier in a checkout: the mechanical part of
the rename, re-runnable and idempotent (docs/DEVELOPMENT.md, *Renaming to
Atelier*).

    python3 tools/rename-to-atelier.py [--check] [--verbose] [PATH...]

Run it at the root of a checkout (a branch made before the rename runs it
before merging main): it renames tracked files and folders whose path names
the old product (`git mv`), then rewrites the text of every tracked text file
(or of the PATHs given). `--check` changes nothing and exits 1 when something
would change (CI-style).

What stays (on purpose):
- what belongs to GitHub Copilot the agent (`AgentKind::Copilot`,
  `copilot_cfg`, `~/.copilot`, `COPILOT_*` variables, "GitHub Copilot CLI",
  `github-copilot/...` model ids): no rule matches them;
- the iPhone app's bundle id `com.thomashorga.copilotcanvas` and the
  `ccanvas://` pairing scheme (no rule matches them);
- the names in KEEP below (the repository URL until the repository is
  renamed, cryptographic context strings that old and new builds must share,
  the owner's Dev Tunnel ids and mock state folders, checkout paths);
- every line holding `rename:keep`, and every line between `rename:keep-begin`
  and `rename:keep-end` (compatibility code and docs that must name the old
  names; in Markdown inside `<!-- ... -->`);
- the files in SKIP (this script, and `canvas-common/src/legacy.rs`, the one
  place the old names live for compatibility).

View snapshots (`*.snap`) are not rewritten: the toolbar's shorter name moves
what follows it, so they are recorded again (`UPDATE_SNAPSHOTS=1 cargo test
-p canvas-core --test view_snapshots`, then read the diffs).
"""

import os
import re
import subprocess
import sys

# Files never rewritten (paths relative to the repository root).
SKIP = {
    "tools/rename-to-atelier.py",
    "crates/canvas-common/src/legacy.rs",
}

# Text kept as it is wherever it appears (longest first is not needed: they
# are masked before any rule runs and restored after).
KEEP = [
    # The repository until the owner renames it on GitHub.
    "TomND/copilot-canvas",
    # Contexts of keys and MACs: old and new builds pair with each other and
    # open secrets sealed before the rename.
    "copilot-canvas pairing v2",
    "copilot-canvas client seal v1",
    "copilot-canvas revoked v1",
    "copilot-canvas remote secret v1",
    "copilot-canvas secret at rest v1",
    # The variable an old client sends in AgentLaunch::env (a wire constant).
    "copilot_canvas_group_skills",
    # User data: the owner's Dev Tunnel ids and the mock services' state folders.
    "copilot-canvas-mac",
    "copilot-canvas-mock",
    # Checkout folders on the owner's machines and on CI runners.
    "repos\\copilot-canvas",
    "repos/copilot-canvas",
    "My project/copilot-canvas",
    "work/copilot-canvas/copilot-canvas",
]

# (pattern, replacement), in order. Rust-only rules come first.
RUST_RULES = [
    # Reads of the canvas's variables go through the helper that falls back
    # to the old name (canvas_common::env).
    (re.compile(r'std::env::var\("(?:COPILOT_CANVAS|ATELIER)_'), '{env}::var("ATELIER_'),
    (re.compile(r'std::env::var_os\("(?:COPILOT_CANVAS|ATELIER)_'), '{env}::var_os("ATELIER_'),
]

RULES = [
    (re.compile(r"\ba Copilot Canvas\b"), "an Atelier"),
    (re.compile(r"\bA Copilot Canvas\b"), "An Atelier"),
    (re.compile(r"Copilot Canvas"), "Atelier"),
    (re.compile(r"Copilot canvas"), "Atelier canvas"),
    (re.compile(r"copilot canvas"), "atelier"),
    (re.compile(r"Copilot_Canvas"), "Atelier"),
    (re.compile(r"CopilotCanvas"), "Atelier"),
    (re.compile(r"COPILOT_CANVAS"), "ATELIER"),
    (re.compile(r"com\.copilot-canvas\."), "com.thomashorga.atelier."),
    (re.compile(r"copilot-canvas"), "atelier"),
    (re.compile(r"copilot_canvas"), "atelier"),
]

# Path rules: the same names in file and folder names.
PATH_RULES = [
    (re.compile(r"CopilotCanvas"), "Atelier"),
    (re.compile(r"copilot-canvas"), "atelier"),
]

TEXT_SUFFIXES = {
    ".rs", ".toml", ".md", ".txt", ".yml", ".yaml", ".json", ".sh", ".py", ".ps1", ".swift", ".plist", ".xml",
    ".desktop", ".rb", ".pbxproj", ".xcscheme", ".xcworkspacedata", ".entitlements", ".html", ".css", ".js",
    ".vtrec", ".lock", ".svg", ".spec", ".in", ".cfg", ".wgsl", ".h", ".c", ".udl", ".rc", ".manifest",
}


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def tracked():
    return [p for p in run("git", "ls-files", "-z").split("\0") if p]


def mask(line):
    """Replaces the kept names by placeholders that no rule matches."""
    saved = []
    for kept in KEEP:
        while kept in line:
            index = len(saved)
            saved.append(kept)
            line = line.replace(kept, f"\x00{index}\x00", 1)
    return line, saved


def unmask(line, saved):
    for index, kept in enumerate(saved):
        line = line.replace(f"\x00{index}\x00", kept, 1)
    return line


def rename_text(path, text):
    rust = path.endswith(".rs")
    env = "crate::env" if path.startswith("crates/canvas-common/") else "canvas_common::env"
    out = []
    keeping = False
    for line in text.splitlines(keepends=True):
        if "rename:keep-begin" in line:
            keeping = True
        if keeping or "rename:keep" in line:
            if "rename:keep-end" in line:
                keeping = False
            out.append(line)
            continue
        body, saved = mask(line)
        if rust:
            for pattern, replacement in RUST_RULES:
                body = pattern.sub(replacement.format(env=env), body)
        for pattern, replacement in RULES:
            body = pattern.sub(replacement, body)
        out.append(unmask(body, saved))
    return "".join(out)


def new_path(path):
    for pattern, replacement in PATH_RULES:
        path = pattern.sub(replacement, path)
    return path


def is_text(path):
    _, suffix = os.path.splitext(path)
    name = os.path.basename(path)
    return suffix.lower() in TEXT_SUFFIXES or name in {"Dockerfile", "Makefile", "LICENSE", ".gitignore", "PKGBUILD", "AppRun"} or suffix == ""


def main(argv):
    check = "--check" in argv
    verbose = "--verbose" in argv
    only = [a for a in argv if not a.startswith("--")]
    root = run("git", "rev-parse", "--show-toplevel").strip()
    os.chdir(root)
    changed = []

    # 1. Paths: files and folders named after the old product.
    files = tracked()
    for path in files:
        if path in SKIP:
            continue
        target = new_path(path)
        if target != path:
            changed.append(f"rename {path} -> {target}")
            if not check:
                os.makedirs(os.path.dirname(target) or ".", exist_ok=True)
                run("git", "mv", "-k", path, target)

    # 2. Text.
    files = only or tracked()
    for path in files:
        if path in SKIP or not os.path.isfile(path) or not is_text(path):
            continue
        with open(path, "rb") as f:
            data = f.read()
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            continue
        renamed = rename_text(path, text)
        if renamed != text:
            changed.append(f"edit {path}")
            if not check:
                with open(path, "wb") as f:
                    f.write(renamed.encode("utf-8"))

    for line in changed if verbose or check else []:
        print(line)
    print(f"{len(changed)} change(s){' needed' if check else ''}")
    return 1 if check and changed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
