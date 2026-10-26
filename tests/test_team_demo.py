"""Synthetic CLI regressions. Set BRAIN_PROBE_BIN or BRAIN_BIN to a built brain."""
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import time
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
BRAIN = os.environ.get("BRAIN_PROBE_BIN") or os.environ.get("BRAIN_BIN")


@unittest.skipUnless(BRAIN, "set BRAIN_PROBE_BIN or BRAIN_BIN")
class TeamDemo(unittest.TestCase):
    def test_two_machines(self):
        env = dict(os.environ, BRAIN_DEMO_BIN=str(Path(BRAIN).resolve()))
        p = subprocess.run([str(REPO / "tools/team-demo.sh"), "--machines", "2"],
                           env=env, capture_output=True, text=True, timeout=115)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        for text in ("Готова worker-1", "Готова worker-2", "enroll → join", "класс task",
                     "класс error", "reopened", "класс decision", "failed", "класс plan",
                     "машина worker-1", "машина worker-2", "факт исчез", "нет ответа",
                     "временные HOME удалены", "Демо завершено",
                     "Какие ошибки сейчас открыты?", "Какие решения не сработали и почему?",
                     "Что в планах на следующий спринт?", "Чем заняты машины?"):
            self.assertIn(text, p.stdout)
        self.assertNotIn("Лексический запрос", p.stdout)

    def test_keep_stops_servers_and_retains_only_synthetic_state(self):
        env = dict(os.environ, BRAIN_DEMO_BIN=str(Path(BRAIN).resolve()))
        p = subprocess.run([str(REPO / "tools/team-demo.sh"), "--machines", "1", "--keep"],
                           env=env, capture_output=True, text=True, timeout=115)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        line = next(line for line in p.stdout.splitlines() if line.startswith("Сохранено (--keep): "))
        base = Path(line.removeprefix("Сохранено (--keep): ").split("; серверы")[0])
        self.addCleanup(shutil.rmtree, base)
        self.assertTrue((base / "worker-1/.brain/brain.sqlite").is_file())
        self.assertTrue((base / "leader/.brain/net/tls/ca.pem").is_file())
        self.assertFalse((base / "worker-1.bundle").exists(), "join consumed bundle")

    def test_sigterm_cleans_started_servers_and_home(self):
        with tempfile.TemporaryDirectory(prefix="brain-demo-interrupt-") as temp:
            env = dict(os.environ, BRAIN_DEMO_BIN=str(Path(BRAIN).resolve()), TMPDIR=temp)
            p = subprocess.Popen([str(REPO / "tools/team-demo.sh"), "--machines", "16"],
                                 env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                deadline = time.monotonic() + 15
                while not list(Path(temp).glob("brain-team-demo-*/leader/server.log")):
                    self.assertIsNone(p.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.05)
                p.send_signal(signal.SIGTERM)
                stdout, stderr = p.communicate(timeout=15)
                self.assertNotEqual(p.returncode, 0)
                self.assertIn("серверы остановлены", stdout, stderr)
                self.assertEqual(list(Path(temp).iterdir()), [])
            finally:
                if p.poll() is None:
                    p.kill()
                    p.wait(timeout=5)
                p.stdout.close()
                p.stderr.close()


@unittest.skipUnless(BRAIN, "set BRAIN_PROBE_BIN or BRAIN_BIN")
class TeamSchedule(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="brain-team-timer-")
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home with spaces"
        self.home.mkdir()
        fake = self.home / "bin"
        fake.mkdir()
        self.log = self.home / "systemctl.log"
        ctl = fake / "systemctl"
        ctl.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$HOME/systemctl.log"\nexit "${FAKE_SYSTEMCTL_EXIT:-0}"\n')
        ctl.chmod(0o755)
        self.env = {"HOME": str(self.home), "PATH": f"{fake}:/usr/bin:/bin", "LANG": "C.UTF-8"}
        self.brain = str(Path(BRAIN).resolve())
        self.units = self.home / ".config/systemd/user"

    def run_brain(self, *args, ok=True):
        p = subprocess.run([self.brain, "--offline", *map(str, args)], env=self.env,
                           cwd=self.home, capture_output=True, text=True, timeout=15)
        self.assertEqual(p.returncode == 0, ok, p.stdout + p.stderr)
        return json.loads(p.stdout) if ok else None

    def configure(self):
        root = self.home / "project"
        root.mkdir()
        self.run_brain("team", "configure", "--machine", "worker-1", "--project", "demo", "--root", root,
                       "--team-space", "team.demo", "--recipient", "leader:lead")
        self.run_brain("team", "enable")

    def test_disabled_refuses_install_and_scheduled_noop(self):
        self.run_brain("team", "schedule", "enable", ok=False)
        self.assertFalse(self.units.exists())
        result = self.run_brain("team", "publish", "--scheduled")
        self.assertEqual(result["status"], "publication_disabled")
        self.assertFalse((self.home / ".brain/net").exists())
        status = self.run_brain("team", "schedule", "status")
        self.assertFalse(status["unit_files_installed"])
        self.assertEqual(status["last_run"]["status"], "publication_disabled")

    def test_pinned_timer_optout_status_and_disable(self):
        self.configure()
        # Pin a nondefault state directory and DB, independent of timer cwd.
        state = self.home / "isolated state"
        (self.home / ".brain/net").rename(state)
        db = self.home / "custom database.sqlite"
        args = ["--database", db, "team", "schedule", "enable", "--state-dir", state, "--every", "15m"]
        self.assertEqual(self.run_brain(*args)["status"], "ok")
        service = (self.units / "brain-team-publish.service").read_text()
        timer = (self.units / "brain-team-publish.timer").read_text()
        self.assertIn("OnUnitInactiveSec=15m", timer)
        self.assertIn(f'Environment="HOME={self.home}"', service)
        self.assertNotIn("/bin/sh", service)
        exec_line = next(line.removeprefix("ExecStart=") for line in service.splitlines() if line.startswith("ExecStart="))
        argv = shlex.split(exec_line)
        self.assertIn(str(db), argv)
        self.assertIn(str(state), argv)
        self.assertIn("team.demo", argv)
        self.run_brain("team", "disable", "--state-dir", state)
        p = subprocess.run(argv, cwd="/", env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(json.loads(p.stdout)["status"], "publication_disabled")
        status = self.run_brain("team", "schedule", "status")
        self.assertTrue(status["enabled"])
        self.assertTrue(status["active"])
        self.assertEqual(status["last_run"]["status"], "publication_disabled")
        self.assertIn("--user enable --now brain-team-publish.timer", self.log.read_text())
        self.run_brain("team", "schedule", "disable")
        self.assertFalse((self.units / "brain-team-publish.service").exists())
        self.assertFalse((self.units / "brain-team-publish.timer").exists())

    def test_scheduled_publish_and_failure_diagnostics(self):
        self.configure()
        self.run_brain("init")
        result = self.run_brain("team", "publish", "--scheduled")
        self.assertEqual(result["status"], "ok")
        status = self.run_brain("team", "schedule", "status")
        self.assertEqual(status["last_run"]["status"], "published")
        self.assertIsInstance(status["last_run"]["at"], int)
        # A corrupt source fails closed and records only a bounded diagnostic.
        (self.home / ".brain/brain.sqlite").write_bytes(b"not a sqlite database")
        self.run_brain("team", "publish", "--scheduled", ok=False)
        status = self.run_brain("team", "schedule", "status")
        self.assertEqual(status["last_run"]["status"], "failed")
        self.assertEqual(set(status["last_run"]), {"at", "status"})

    def test_invalid_interval_and_systemctl_failure(self):
        self.configure()
        for interval in ("0m", "1m\n", "1;sh", "é", "10001m"):
            self.run_brain("team", "schedule", "enable", "--every", interval, ok=False)
        self.assertFalse(self.units.exists())
        self.env["FAKE_SYSTEMCTL_EXIT"] = "1"
        result = self.run_brain("team", "schedule", "enable")
        self.assertEqual(result["status"], "files_written_systemctl_failed")
        status = self.run_brain("team", "schedule", "status")
        self.assertFalse(status["enabled"])
        self.assertFalse(status["active"])


if __name__ == "__main__":
    unittest.main()
