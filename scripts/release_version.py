#!/usr/bin/env python3
"""Validate one canonical package SemVer and classify its prerelease state."""

from pathlib import Path
import runpy
import sys


def classify(version: str) -> str:
    pack = runpy.run_path(str(Path(__file__).with_name("pack.py")))
    canonical_semver = pack["canonical_semver"]
    if not canonical_semver(version):
        raise SystemExit("release version is not canonical SemVer")
    release_without_build = version.split("+", maxsplit=1)[0]
    return "true" if "-" in release_without_build else "false"


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: release_version.py VERSION")
    print(classify(sys.argv[1]))


if __name__ == "__main__":
    main()
