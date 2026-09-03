"""Regression tests for the stable S3 documentation contract."""

import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location(
    "check_compatibility", ROOT / "scripts/check-compatibility.py"
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load check-compatibility.py")
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class ReadmeCompatibilityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.readme = (ROOT / "README.md").read_text(encoding="utf-8")

    def test_current_stable_claims_pass(self) -> None:
        CHECK.check_readme(self.readme)

    def test_obsolete_rc_floor_cannot_coexist_with_stable_floor(self) -> None:
        contradictory = (
            self.readme
            + "\nVersion 0.3.0 requires Sigil 0.33.2-rc.1 or newer and Host API 1.2.\n"
        )
        with self.assertRaisesRegex(ValueError, "stale compatibility claim"):
            CHECK.check_readme(contradictory)

    def test_retired_source_candidate_guidance_is_rejected(self) -> None:
        stale_guidance = (
            "The accepted 0.3.0 stable source candidate is ready.",
            (
                "Public stable 0.1.0 remains current while 0.3.0 is prepared "
                "from the accepted immutable prerelease."
            ),
            (
                "Do not add s3@0.3.0 until the separately authorized stable "
                "release is published and verified."
            ),
            "| Public stable 0.1.0 | Accepted 0.3.0 stable source candidate |",
            "The 0.3 prerelease adds only list-objects and Host API 1.2.",
        )
        for guidance in stale_guidance:
            with self.subTest(guidance=guidance):
                with self.assertRaisesRegex(ValueError, "stale compatibility claim"):
                    CHECK.check_readme(f"{self.readme}\n{guidance}\n")


if __name__ == "__main__":
    unittest.main()
