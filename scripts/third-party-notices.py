"""Writes THIRD-PARTY-NOTICES.txt: every third-party crate the `cookie`
binary and the web client are built from, with its version, license, and
source, the license texts those crates ship, and the web client's bundled
font, icons, and highlighter.

Release archives include the file, the daemon serves it beside the web
page, and the TUI's /about shows it. Run it after changing dependencies;
CI runs it with --check, which fails when the committed file is stale:

    python3 scripts/third-party-notices.py
    python3 scripts/third-party-notices.py --check
"""

import hashlib
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "THIRD-PARTY-NOTICES.txt")

# (workspace package, target) pairs whose normal dependencies ship.
SHIPPED = [
    ("cookie_agent", "all"),
    ("cookie_agent_web", "wasm32-unknown-unknown"),
]

# Files in a crate's package that hold its license or notices.
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense")

# What the web client bundles besides crates: (what, license, file in the
# repository holding its license, or None, and where it comes from).
ASSETS = [
    (
        "Font Awesome Free (icons), by Fonticons, Inc.",
        "CC-BY-4.0",
        None,
        "https://fontawesome.com/license/free",
    ),
    (
        "JetBrains Mono (code font), by JetBrains s.r.o.",
        "OFL-1.1",
        "crates/web/assets/fonts/JetBrainsMono-OFL.txt",
        "https://github.com/JetBrains/JetBrainsMono",
    ),
    (
        "highlight.js (code highlighting)",
        "BSD-3-Clause",
        "crates/web/assets/vendor/highlight.js-LICENSE.txt",
        "https://github.com/highlightjs/highlight.js",
    ),
]


def cargo(*args):
    return subprocess.run(
        ["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout


def shipped_ids():
    """`(name, version)` of every crate in a shipped dependency tree."""
    ids = set()
    for package, target in SHIPPED:
        tree = cargo(
            "tree", "--locked", "--edges", "normal", "--prefix", "none",
            "--format", "{p}", "--package", package, "--target", target,
        )
        for line in tree.splitlines():
            # `name v1.2.3`, then ` (path)` for workspace crates or ` (*)`
            # for one printed before.
            parts = line.split()
            if len(parts) >= 2 and parts[1].startswith("v"):
                ids.add((parts[0], parts[1][1:]))
    return ids


def license_texts(package):
    """The license files a crate's package holds, as `(file name, text)`."""
    directory = os.path.dirname(package["manifest_path"])
    names = set()
    for name in os.listdir(directory):
        if name.lower().startswith(LICENSE_PREFIXES) and os.path.isfile(
            os.path.join(directory, name)
        ):
            names.add(name)
    if package.get("license_file"):
        names.add(os.path.relpath(
            os.path.join(directory, package["license_file"]), directory
        ))
    texts = []
    for name in sorted(names):
        path = os.path.join(directory, name)
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as file:
                texts.append((name, file.read().strip()))
    return texts


MIT_TEMPLATE = """MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""


def standard_text(package):
    """The standard text of the license a crate without license files names:
    MIT, with its authors as the holders, wherever MIT is one of the
    choices; otherwise Apache-2.0, whose text names no holder."""
    choices = re.split(r"[\s/()]+", package.get("license") or "")
    if "MIT" in choices:
        authors = [author.split("<")[0].strip() for author in package.get("authors", [])]
        holders = ", ".join(author for author in authors if author) or (
            f"the {package['name']} authors"
        )
        return "MIT (standard text)", MIT_TEMPLATE.format(holders=holders)
    if "Apache-2.0" in choices:
        with open(os.path.join(ROOT, "LICENSE-APACHE"), encoding="utf-8") as file:
            return "Apache-2.0 (standard text)", file.read().strip()
    return None


def render():
    metadata = json.loads(cargo("metadata", "--locked", "--format-version", "1"))
    workspace = set(metadata["workspace_members"])
    shipped = shipped_ids()
    crates = sorted(
        (
            package
            for package in metadata["packages"]
            if package["id"] not in workspace
            and (package["name"], package["version"]) in shipped
        ),
        key=lambda package: (package["name"], package["version"]),
    )
    # Identical license texts are printed once, under every crate they
    # cover.
    groups = {}
    order = []
    missing = []
    for package in crates:
        label = f"{package['name']} {package['version']}"
        texts = license_texts(package)
        if not texts:
            standard = standard_text(package)
            if standard is None:
                missing.append(package)
            else:
                texts = [standard]
        for name, text in texts:
            digest = hashlib.sha256(text.encode()).hexdigest()
            if digest not in groups:
                groups[digest] = {"text": text, "crates": [], "files": set()}
                order.append(digest)
            groups[digest]["crates"].append(label)
            groups[digest]["files"].add(name)

    lines = [
        "Third-party notices",
        "",
        "cookie is built from the third-party software listed here, each",
        "under its own license. cookie's own license is in LICENSE.",
        "",
        "This file is generated by scripts/third-party-notices.py.",
        "",
        "=" * 79,
        "Bundled with the web client",
        "=" * 79,
        "",
    ]
    for what, license, _, source in ASSETS:
        lines.append(f"{what}: {license}, {source}")
    lines += ["", "=" * 79, f"Rust crates ({len(crates)})", "=" * 79, ""]
    for package in crates:
        source = package.get("repository") or f"https://crates.io/crates/{package['name']}"
        license = package.get("license") or "see its license file"
        lines.append(f"{package['name']} {package['version']}: {license}, {source}")
    for what, license, path, _ in ASSETS:
        if path is None:
            continue
        with open(os.path.join(ROOT, path), encoding="utf-8") as file:
            text = file.read().strip()
        lines += ["", "=" * 79, f"{what} ({license})", "=" * 79, "", text]
    for digest in order:
        group = groups[digest]
        lines += [
            "",
            "=" * 79,
            ", ".join(sorted(group["files"])) + " of:",
        ]
        lines += [f"  {label}" for label in group["crates"]]
        lines += ["=" * 79, "", group["text"]]
    if missing:
        lines += [
            "",
            "=" * 79,
            "Crates whose package ships no license file and names no",
            "license with a standard text here; see their source above:",
            "=" * 79,
            "",
        ]
        lines += [f"  {package['name']} {package['version']}" for package in missing]
    return "\n".join(lines) + "\n"


def main():
    text = render()
    if "--check" in sys.argv[1:]:
        try:
            with open(OUT, encoding="utf-8") as file:
                current = file.read()
        except FileNotFoundError:
            current = None
        if current != text:
            print(
                "THIRD-PARTY-NOTICES.txt is stale: run "
                "`python3 scripts/third-party-notices.py` and commit the result",
                file=sys.stderr,
            )
            sys.exit(1)
        return
    with open(OUT, "w", encoding="utf-8") as file:
        file.write(text)


if __name__ == "__main__":
    main()
