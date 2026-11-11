#!/usr/bin/env python3
"""Write THIRD_PARTY_LICENSES.txt for the macOS viewer build.

Walks the dependency graph from `cargo metadata` starting at the
`sccm-rc-viewer` package, following only *normal* dependencies (no dev- or
build-dependencies, which are not shipped) for the macOS targets, and copies
each crate's license / notice files from the local Cargo cache. Crates that
ship no license file are listed with their SPDX expression and repository so
the gap is visible instead of silently dropped.

Usage: third_party_licenses.py OUTPUT_FILE
"""
import json
import os
import subprocess
import sys

ROOT_PACKAGE = "sccm-rc-viewer"
TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin"]
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "copyright", "unlicense")
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

MIT_TEXT = """Copyright (c) {holders}

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
SOFTWARE.
"""


def synthesized_text(pkg):
    """Standard license text for a crate that ships no license file, chosen
    from its SPDX expression (for `A OR B` any one may be used)."""
    expr = (pkg.get("license") or "").replace("/", " OR ")
    ids = {t.strip("() ") for t in expr.split()}
    if "MIT" in ids:
        authors = [a.split(" <")[0] for a in pkg.get("authors") or []]
        holders = ", ".join(authors) or f"the {pkg['name']} authors"
        return "MIT License (standard text; the crate ships no license file)\n\n" + MIT_TEXT.format(
            holders=holders
        )
    if "Apache-2.0" in ids:
        return "Licensed under the Apache License, Version 2.0; full text in the appendix below.\n"
    if "CC0-1.0" in ids:
        return "Dedicated to the public domain under CC0-1.0; no attribution required.\n"
    return None


def metadata(target):
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)


def shipped_packages(meta):
    """Package ids reachable from the viewer via normal dependencies."""
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == ROOT_PACKAGE)
    seen, stack = set(), [root]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    return seen


def license_files(manifest_path):
    d = os.path.dirname(manifest_path)
    files = []
    for name in sorted(os.listdir(d)):
        full = os.path.join(d, name)
        if os.path.isfile(full) and name.lower().startswith(LICENSE_PREFIXES):
            files.append(full)
    return files


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out_path = sys.argv[1]

    packages, members = {}, set()
    for target in TARGETS:
        meta = metadata(target)
        members |= set(meta["workspace_members"])
        by_id = {p["id"]: p for p in meta["packages"]}
        for pid in shipped_packages(meta):
            packages[pid] = by_id[pid]

    third_party = sorted(
        (p for pid, p in packages.items() if pid not in members),
        key=lambda p: (p["name"], p["version"]),
    )

    missing = []
    with open(out_path, "w", encoding="utf-8") as out:
        out.write("Third-party software in SCCM RC for macOS\n")
        out.write("=" * 41 + "\n\n")
        out.write(
            "SCCM RC is licensed under MIT OR Apache-2.0 (see LICENSE-MIT and\n"
            "LICENSE-APACHE in the source repository). It includes the following\n"
            f"third-party crates ({len(third_party)}), each under its own license:\n\n"
        )
        for p in third_party:
            out.write(f"  {p['name']} {p['version']}  ({p.get('license') or 'see below'})\n")
        for p in third_party:
            out.write("\n" + "-" * 72 + "\n")
            out.write(f"{p['name']} {p['version']}\n")
            out.write(f"License: {p.get('license') or p.get('license_file') or 'unspecified'}\n")
            if p.get("repository"):
                out.write(f"Source: {p['repository']}\n")
            files = license_files(p["manifest_path"])
            if not files:
                text = synthesized_text(p)
                if text is None:
                    missing.append(f"{p['name']} {p['version']}")
                    text = "(No license file is shipped with this crate; see its source repository.)\n"
                out.write("\n" + text)
            for f in files:
                out.write(f"\n--- {os.path.basename(f)} ---\n\n")
                with open(f, encoding="utf-8", errors="replace") as fh:
                    out.write(fh.read().rstrip() + "\n")

        out.write("\n" + "=" * 72 + "\nAppendix: Apache License, Version 2.0\n" + "=" * 72 + "\n\n")
        with open(os.path.join(REPO_ROOT, "LICENSE-APACHE"), encoding="utf-8") as fh:
            out.write(fh.read())

    print(f"{out_path}: {len(third_party)} crates, {len(missing)} without a known license text")
    for m in missing:
        print(f"  no license file: {m}")


if __name__ == "__main__":
    main()
