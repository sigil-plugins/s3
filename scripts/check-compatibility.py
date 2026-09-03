#!/usr/bin/env python3
"""Keep the stable schema-3 S3 compatibility documentation truthful."""

from pathlib import Path
import tomllib


ROOT = Path(__file__).resolve().parent.parent
EXPECTED_SIGIL = ">=0.33.2-rc.1, <1.0.0"


def main() -> None:
    manifest = tomllib.loads((ROOT / "plugin.toml").read_text(encoding="utf-8"))
    readme = (ROOT / "README.md").read_text(encoding="utf-8")

    expected = {
        "version": "0.3.0",
        "schema_version": 3,
        "host_api": "^1.2",
        "sigil": EXPECTED_SIGIL,
    }
    observed = {
        "version": manifest["version"],
        "schema_version": manifest["schema_version"],
        "host_api": manifest["requires"]["host_api"],
        "sigil": manifest["requires"]["sigil"],
    }
    if observed != expected:
        raise SystemExit(f"incompatible schema-3 contract: {observed!r} != {expected!r}")

    for claim in (
        "requires stable Sigil 0.33.2 or newer and Host API 1.2",
        "first appeared in the 0.33.2-rc.1 prerelease",
        "Sigil 0.33.1 predates manifest schema 3",
        "cannot load it",
        "sigil plugin add s3@0.3.0",
    ):
        if claim not in readme:
            raise SystemExit(f"README is missing compatibility claim: {claim!r}")


if __name__ == "__main__":
    main()
