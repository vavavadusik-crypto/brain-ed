"""docs/cli-reference.md must match the binary's own --help.

Runs the same generator in --check mode: it rebuilds the reference from the
built binary and compares it byte-for-byte with the committed file. Stale
documentation is a failure, exactly like other generated-file checks.

Requires the debug binary (CARGO_TARGET_DIR or ./target/debug), like the
viewer tests that drive a built brain.
"""
import os
import pathlib
import subprocess
import unittest


class CliReferenceTest(unittest.TestCase):
    def test_committed_reference_is_not_stale(self):
        root = pathlib.Path(__file__).resolve().parent.parent
        binary = (
            os.environ.get("BRAIN_REF_BIN")
            or pathlib.Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
            / "debug"
            / "brain"
        )
        self.assertTrue(pathlib.Path(binary).is_file(),
                        f"build brain first, looked at {binary}")
        result = subprocess.run(
            ["python3", str(root / "tools" / "gen-cli-reference.py"), "--check"],
            capture_output=True, text=True,
            env={**os.environ, "BRAIN_REF_BIN": str(binary), "BRAIN_OFFLINE": "true"},
        )
        self.assertEqual(result.returncode, 0, result.stderr or "reference is stale")

    def test_reference_covers_every_top_level_command(self):
        root = pathlib.Path(__file__).resolve().parent.parent
        text = (root / "docs" / "cli-reference.md").read_text(encoding="utf-8")
        binary = (
            os.environ.get("BRAIN_REF_BIN")
            or pathlib.Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
            / "debug"
            / "brain"
        )
        help_text = subprocess.run([str(binary), "--help"], capture_output=True,
                                   text=True).stdout
        for line in help_text.splitlines():
            match = None
            if line.startswith("  ") and not line.startswith("   "):
                parts = line.strip().split(None, 1)
                if parts and parts[0][0].islower():
                    match = parts[0]
            if match and match not in ("help",):
                self.assertIn(f"`brain {match}`", text,
                              f"the reference has no page for {match}")


if __name__ == "__main__":
    unittest.main()
