"""The exam wrapper must never forward private response text or stderr."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


WRAPPER = Path(__file__).resolve().parents[1] / "bench/lexical-exam.py"


class LexicalExamPrivacyTest(unittest.TestCase):
    def run_fixture(self, broken=False, linked=False):
        with tempfile.TemporaryDirectory(prefix="e31-exam-test-") as directory:
            root = Path(directory)
            home = root / "home"
            (home / ".brain").mkdir(parents=True)
            (home / ".brain/brain.sqlite").touch()
            if linked:
                outside = root / "outside.sqlite"
                outside.touch()
                (home / ".brain/brain.sqlite").unlink()
                (home / ".brain/brain.sqlite").symlink_to(outside)
            (home / ".e31-disposable").touch()
            probe = root / "probe"
            probe.write_text("#!/usr/bin/env python3\nprint('[{\"terms\":[\"alpha\"],\"prefixes\":[\"alph\"]}]')\n")
            probe.chmod(0o700)
            binary = root / "brain"
            document = {"results": [{"text": "alpha PRIVATE_RESPONSE_CANARY",
                                      "source_agent": "PRIVATE_SOURCE_CANARY"}]}
            payload = "malformed PRIVATE_RESPONSE_CANARY" if broken else json.dumps(document)
            binary.write_text("#!/usr/bin/env python3\nimport sys\n"
                              "print('PRIVATE_STDERR_CANARY', file=sys.stderr)\n"
                              f"print({payload!r})\n")
            binary.chmod(0o700)
            exam = root / "exam.py"
            exam.write_text('''import json, subprocess
EXAM = [("E1", "alpha", [], None)]
def answers(doc):
    return [{"text": result["text"]} for result in doc["results"]]
def judge(top, patterns, period, doc):
    return bool(top)
def main():
    run = subprocess.run([BRAIN, "ask", "--budget-tokens", BUDGET[0], "alpha"],
                         capture_output=True, text=True)
    if run.returncode == 0:
        print(run.stdout)
    print("PRIVATE_EXAM_OUTPUT_CANARY")
''')
            return subprocess.run([sys.executable, str(WRAPPER), str(binary),
                                   "--home", str(home), "--models", str(root / "models"),
                                   "--exam", str(exam), "--probe", str(probe)],
                                  capture_output=True, text=True)

    def test_success_reports_only_counts_and_booleans(self):
        run = self.run_fixture()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertNotIn("PRIVATE_", run.stdout + run.stderr)
        rows = [json.loads(line) for line in run.stdout.splitlines()]
        self.assertTrue(rows[0]["hit"])
        self.assertEqual(rows[0]["literal_noise"], 0)
        self.assertEqual(rows[-1]["questions"], 1)

    def test_malformed_response_is_content_free_and_fails_the_run(self):
        run = self.run_fixture(broken=True)
        self.assertEqual(run.returncode, 1)
        self.assertNotIn("PRIVATE_", run.stdout + run.stderr)
        self.assertTrue(json.loads(run.stdout.splitlines()[0])["error"])

    def test_link_outside_the_disposable_home_is_rejected(self):
        run = self.run_fixture(linked=True)
        self.assertEqual(run.returncode, 2)
        self.assertIn("snapshot paths must stay inside", run.stderr)
        self.assertNotIn("PRIVATE_", run.stdout + run.stderr)


if __name__ == "__main__":
    unittest.main()
