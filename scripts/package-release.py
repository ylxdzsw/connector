#!/usr/bin/env python3
"""Stage supplied native artifacts for upload; never build or overwrite a release."""

import argparse
import hashlib
import json
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="1–64 ASCII letters, digits, dots, underscores or hyphens; start with a letter or digit")
    parser.add_argument("--linux", required=True, type=Path, help="existing static Linux x86-64 binary")
    parser.add_argument("--windows", type=Path, help="optional rebuilt Windows x86-64 .exe")
    parser.add_argument("--output", required=True, type=Path, help="new staging directory; must not already exist")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}", args.version):
        parser.error("invalid version")
    sources = {
        "linux-x86_64": (args.linux, "connector-client-linux-x86_64"),
    }
    if args.windows:
        sources["windows-x86_64"] = (args.windows, "connector-client-windows-x86_64.exe")
    for source, _ in sources.values():
        if not source.is_file() or source.stat().st_size == 0:
            parser.error(f"artifact must be a nonempty regular file: {source}")
    if args.output.exists() or args.output.is_symlink():
        parser.error("output already exists; use a new staging directory")

    args.output.mkdir(parents=True)
    release = args.output / "releases" / args.version
    release.mkdir(parents=True)
    manifest = {"version": args.version, "artifacts": {}}
    for platform, (source, filename) in sources.items():
        destination = release / filename
        digest = hashlib.sha256()
        with source.open("rb") as src, destination.open("xb") as dst:
            while chunk := src.read(1024 * 1024):
                dst.write(chunk)
                digest.update(chunk)
        destination.chmod(0o755 if platform == "linux-x86_64" else 0o644)
        manifest["artifacts"][platform] = {
            "key": f"releases/{args.version}/{filename}",
            "sha256": digest.hexdigest(),
        }
    content = json.dumps(manifest, indent=2) + "\n"
    with (release / "manifest.json").open("x") as output:
        output.write(content)
    # The mutable upload pointer is staged only after all immutable files exist.
    with (args.output / "stable.json").open("x") as output:
        output.write(content)
    print(args.output / "stable.json")


if __name__ == "__main__":
    main()
