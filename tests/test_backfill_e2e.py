"""End to end: the new binary's `archive backfill start` admits an old synthetic
conversation, and the embedded collector then brings it into the archive.

    BRAIN_BIN=/path/to/new/brain python3 -I tests/test_backfill_e2e.py

Synthetic HOME only; no live home, no network, no real transcript.
"""
import json
import os
import shutil
import subprocess
import tempfile
import unittest

BRAIN = os.environ.get("BRAIN_BIN")


def jl(*objs):
    return "".join(json.dumps(o, ensure_ascii=False) + "\n" for o in objs)


def claude(text, uuid, session="old-sess"):
    return {"type": "user", "sessionId": session, "uuid": uuid, "cwd": "/work/old",
            "timestamp": "2026-09-01T10:00:00Z",
            "message": {"role": "user", "content": text}}


@unittest.skipUnless(BRAIN, "set BRAIN_BIN to the brain binary under test")
class BackfillEndToEnd(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.mkdtemp(prefix="brain-bf-e2e-")
        os.chmod(self.home, 0o700)
        self.env = {"HOME": self.home, "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
                    "BRAIN_OFFLINE": "true"}
        # One recent (in-window) file and one old (out-of-window) file.
        recent = os.path.join(self.home, ".claude/projects/-work-recent/r.jsonl")
        os.makedirs(os.path.dirname(recent))
        with open(recent, "w", encoding="utf-8") as handle:
            handle.write(jl(claude("недавнее_слово", "r1", "recent-sess")))
        self.old = os.path.join(self.home, ".claude/projects/-work-old/o.jsonl")
        os.makedirs(os.path.dirname(self.old))
        with open(self.old, "w", encoding="utf-8") as handle:
            handle.write(jl(claude("старое_слово_e2e", "o1")))
        import time
        past = time.time() - 40 * 86400
        os.utime(self.old, (past, past))

    def tearDown(self):
        shutil.rmtree(self.home, ignore_errors=True)

    def brain(self, *args, ok=True):
        proc = subprocess.run([BRAIN, *args], env=self.env, capture_output=True, text=True,
                              timeout=180)
        if ok:
            self.assertEqual(proc.returncode, 0, proc.stderr)
            return json.loads(proc.stdout)
        return proc

    def test_cli_start_admits_old_conversation_through_embedded_collector(self):
        self.brain("archive", "enable", "--since-days", "1")
        # Before start: the old file is out of window and not collected.
        first = self.brain("archive", "collect")
        self.assertEqual(first["status"], "ok")
        self.assertEqual(len(self.brain("archive", "search", "старое_слово_e2e")["results"]), 0)
        self.assertEqual(len(self.brain("archive", "search", "недавнее_слово")["results"]), 1)

        status = self.brain("archive", "backfill", "status")["backfill"]
        self.assertIn(status["state"], ("disabled", "running"))

        # Explicit CLI start.
        started = self.brain("archive", "backfill", "start", "--files-per-run", "2")
        self.assertEqual((started["state"], started["files_per_run"]), ("running", 2))

        # The embedded collector now admits the old file and stores its text.
        collected = self.brain("archive", "collect")
        self.assertEqual(collected["status"], "ok")
        self.assertEqual(collected["clients"]["claude"]["admitted"], 1)
        self.assertEqual(collected["clients"]["claude"]["out_of_window"], 0)
        hits = self.brain("archive", "search", "старое_слово_e2e")["results"]
        self.assertEqual(len(hits), 1)
        self.assertEqual(hits[0]["client"], "claude")

        # Status separates the read-contract stored count from the last-run counts.
        document = self.brain("archive", "backfill", "status")
        self.assertFalse(document["complete_capture_claimed"])
        self.assertEqual(document["backfill"]["state"], "running")
        coverage = document["coverage"]
        # stored_visible_sources counts every source with visible text (regular + old).
        self.assertEqual(coverage["stored_visible_sources_by_client"]["claude"], 2)
        # last_run_* describe only what the run recorded; the historic import is 1.
        self.assertEqual(coverage["last_run_admitted_old_files"], 1)
        self.assertIn("last_run_selected_old_files", coverage)
        # The mislabeled historic count key is gone.
        self.assertNotIn("admitted_sources_by_client", coverage)
        # Config-vs-observed: enabled separately from the last real run.
        self.assertTrue(document["collector_enabled"])
        self.assertIsNotNone(document["observed_last_run"])

        # Stop admits nothing new but keeps what is tracked.
        stopped = self.brain("archive", "backfill", "stop")
        self.assertEqual(stopped["state"], "stopped")
        after = self.brain("archive", "collect")
        self.assertEqual(after["clients"]["claude"]["admitted"], 0)
        self.assertEqual(len(self.brain("archive", "search", "старое_слово_e2e")["results"]), 1)

    def test_cli_start_refuses_bad_quota_and_a_malformed_file_without_repairing_it(self):
        self.brain("archive", "enable", "--since-days", "1")
        for bad in ("0", "21", "-1"):
            proc = self.brain("archive", "backfill", "start", "--files-per-run", bad, ok=False)
            self.assertNotEqual(proc.returncode, 0, bad)
        self.brain("archive", "backfill", "start", "--files-per-run", "2")
        backfill = os.path.join(self.home, ".brain/archive/backfill.json")
        broken = b'{"v":1,"files_per_run":99,"state":"running"}\n'
        with open(backfill, "wb") as handle:
            handle.write(broken)
        os.chmod(backfill, 0o600)
        # A malformed file is reported, not repaired: start must refuse and its bytes stay.
        proc = self.brain("archive", "backfill", "start", "--files-per-run", "3", ok=False)
        self.assertNotEqual(proc.returncode, 0)
        with open(backfill, "rb") as handle:
            self.assertEqual(handle.read(), broken, "start must not overwrite a malformed config")
        self.assertEqual(self.brain("archive", "backfill", "status")["backfill"]["state"], "error")


if __name__ == "__main__":
    unittest.main()
