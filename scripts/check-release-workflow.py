#!/usr/bin/env python3
"""Guard prerelease metadata required by Sigil's fail-closed resolver."""

from pathlib import Path


def main() -> None:
    workflow = Path(".github/workflows/publish-release.yml").read_text(encoding="utf-8")
    required = {
        "the dispatch version accepts a prerelease suffix":
            r"(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$",
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


if __name__ == "__main__":
    main()
