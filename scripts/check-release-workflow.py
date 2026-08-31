#!/usr/bin/env python3
"""Guard prerelease metadata required by Sigil's fail-closed resolver."""

from pathlib import Path
import subprocess
import sys


def classify(version: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-I", "scripts/release_version.py", version],
        check=False,
        capture_output=True,
        text=True,
    )


def main() -> None:
    workflow = Path(".github/workflows/publish-release.yml").read_text(encoding="utf-8")
    required = {
        "the dispatch shares canonical package SemVer validation":
            'python3 -I scripts/release_version.py "$VERSION"',
        "GitHub release creation receives the prerelease flag":
            "prerelease_flag=(--prerelease)",
        "draft metadata must match the SemVer identity":
            ".isDraft == true and .isPrerelease == $prerelease",
        "public metadata must still match after immutability":
            ".isDraft == false and .isPrerelease == $prerelease",
    }
    missing = [message for message, snippet in required.items() if snippet not in workflow]
    if missing:
        raise SystemExit("release workflow contract failed: " + "; ".join(missing))

    accepted = {
        "0.2.0": "false",
        "0.2.0-rc.1": "true",
        "0.2.0-0": "true",
        "0.2.0+build.1": "false",
    }
    for version, expected in accepted.items():
        result = classify(version)
        if result.returncode != 0 or result.stdout.strip() != expected:
            raise SystemExit(f"release version classifier rejected {version}")
    for version in ("0.2.0-01", "0.2.0-rc.01", "v0.2.0", "not-semver"):
        if classify(version).returncode == 0:
            raise SystemExit(f"release version classifier accepted {version}")


if __name__ == "__main__":
    main()
