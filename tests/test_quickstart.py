"""The README quick-start block must keep working.

Extracts the commands between the <!-- quickstart:begin/end --> markers of
README.md and runs them in a temporary HOME, offline, with the locally built
binary standing in for the downloaded release. Two honest substitutions, both
documented here and mirrored by assertions:

  * `git clone` / `cd` — no network: the current checkout IS the source.
  * `install.sh --version …` (the real download) — replaced by the installer's
    own `--dry-run` plan plus a copy of the built binary into the same
    destination the plan names (~/.local/bin/brain).

`brain ui` is started, polled for its ready line and terminated. Everything
else runs exactly as written. If the README block is edited without updating
this test, the desync check fails and names the drifted commands.
"""
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
README = ROOT / "README.md"
BIN = pathlib.Path(
    os.environ.get("BRAIN_PROBE_BIN", str(ROOT / "target" / "debug" / "brain"))
)

# The exact command signatures the quick-start block must contain; a block
# that gained or lost a command has to be reflected here on purpose.
EXPECTED_COMMANDS = [
    "git clone ",
    "cd brain-ed",
    "install.sh --version ",
    "--dry-run",
    'export PATH="$HOME/.local/bin:$PATH"',
    "brain init --agents --dry-run",
    "brain init --agents",
    "brain remember ",
    "brain --offline recall ",
    "brain ui",
]


def json_of(mixed: str):
    """Parse the JSON document printed after possible warning lines."""
    start = mixed.index("{")
    try:
        return json.loads(mixed[start:])
    except json.JSONDecodeError as error:
        raise AssertionError(
            f"no JSON document in command output (from char {start}):"
            f" {mixed[max(0, start - 120):start + 200]!r}"
        ) from error


def read_quickstart_lines():
    text = README.read_text(encoding="utf-8")
    match = re.search(
        r"<!-- quickstart:begin -->\s*```sh\n(.*?)\n```\s*<!-- quickstart:end -->",
        text,
        re.DOTALL,
    )
    if not match:
        raise AssertionError(
            "README.md lost the <!-- quickstart:begin/end --> markers around"
            " the quick-start block; restore them or update tests/test_quickstart.py"
        )
    return [line.strip() for line in match.group(1).splitlines() if line.strip()]


class QuickstartTest(unittest.TestCase):
    def setUp(self):
        # The debug binary is large; tmpfs /tmp has a hard size limit and
        # `brain init --agents` copies it into ~/.brain/bin. Temp HOMEs live
        # on the big disk instead.
        base = pathlib.Path.home() / "tmp-brain-tests"
        base.mkdir(exist_ok=True)
        self.home = pathlib.Path(tempfile.mkdtemp(prefix="quickstart-", dir=str(base)))
        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "BRAIN_OFFLINE": "true",
        }
        if not BIN.is_file():
            raise AssertionError(f"build brain first, looked at {BIN}")
        self.lines = read_quickstart_lines()
        joined = "\n".join(self.lines)
        for signature in EXPECTED_COMMANDS:
            self.assertIn(
                signature,
                joined,
                f"README quick-start drifted: expected {signature!r}; update"
                " tests/test_quickstart.py together with the README block",
            )
        # The clone step: the checkout under test is the source, no network.
        self.cwd = ROOT

    def tearDown(self):
        shutil.rmtree(self.home, ignore_errors=True)

    def run_line(self, line, check=True, timeout=120):
        result = subprocess.run(
            line,
            shell=True,
            cwd=self.cwd,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if check:
            self.assertEqual(
                result.returncode, 0, f"{line}\nstdout: {result.stdout}\nstderr: {result.stderr}"
            )
        return result

    def test_quickstart_block_works_offline(self):
        installed = self.home / ".local" / "bin" / "brain"
        dry_run_plan = None
        for line in self.lines:
            if line.startswith("#"):
                continue
            if line.startswith("git clone "):
                # No network in tests; assert the URL still names this project.
                self.assertIn("brain-ed.git", line)
                continue
            if line == "cd brain-ed":
                self.cwd = ROOT
                continue
            if "install.sh" in line and "--dry-run" in line:
                dry_run_plan = self.run_line(line).stdout
                self.assertIn(
                    ".local/bin/brain",
                    dry_run_plan,
                    "the installer plan changed its destination; update this test",
                )
                continue
            if "install.sh" in line:
                # The real download is out of scope offline: put the built
                # binary exactly where the plan says it would go.
                self.assertIsNotNone(
                    dry_run_plan, "the plain install must come after --dry-run"
                )
                installed.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(BIN, installed)
                continue
            if line.startswith("export PATH="):
                self.env["PATH"] = f"{installed.parent}:{self.env['PATH']}"
                continue
            if line.startswith("brain remember"):
                document = json_of(self.run_line(line).stdout)
                self.assertEqual(document["status"], "remembered")
                self.assertTrue(document["id"].startswith("mem_"))
                continue
            if line.startswith("brain --offline recall"):
                document = json_of(self.run_line(line).stdout)
                self.assertGreaterEqual(document["found"], 1)
                texts = json.dumps(document, ensure_ascii=False)
                self.assertIn("cargo --locked", texts)
                continue
            if line == "brain ui":
                self.start_ui(line)
                continue
            # brain init --agents --dry-run / brain init --agents: no agent
            # homes exist in the temp HOME, so both are no-ops that succeed.
            self.run_line(line)

    def start_ui(self, line):
        process = subprocess.Popen(
            line,
            shell=True,
            cwd=self.cwd,
            env=self.env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            # shell=True puts `brain ui` under /bin/sh: signal the whole group,
            # or the server outlives the test as an orphan.
            start_new_session=True,
        )
        try:
            # The ready document is pretty-printed across several lines.
            lines = []
            document = None
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                line = process.stdout.readline()
                if not line:
                    break
                lines.append(line)
                try:
                    document = json.loads("".join(lines))
                    break
                except json.JSONDecodeError:
                    continue
            self.assertIsNotNone(document, f"ui never printed a ready JSON: {lines}")
            self.assertIn("url", document, f"ui ready without url: {document}")
            self.assertIn("127.0.0.1", document["url"])
        finally:
            if process.stdin:
                process.stdin.close()
            group = os.getpgid(process.pid)
            os.killpg(group, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(group, signal.SIGKILL)
                process.wait(timeout=10)
            # The group must be gone, not just the shell.
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    os.killpg(group, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.1)
            else:
                os.killpg(group, signal.SIGKILL)
                self.fail("brain ui outlived the test")
            if process.stdout:
                process.stdout.close()


if __name__ == "__main__":
    unittest.main()
