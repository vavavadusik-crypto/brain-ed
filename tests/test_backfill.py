"""Historic archive backfill: optional admission of old files, honest coverage.

Runs with the stdlib only, like the collector tests:

    python3 -I tests/test_backfill.py

All fixtures are synthetic. Nothing here reads a private database, the network,
paid APIs or any real user transcript.
"""
import contextlib
import importlib.util
import io
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parents[1] / "tools" / "session_archive.py"
_spec = importlib.util.spec_from_file_location("session_archive", MODULE_PATH)
sa = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sa)

DAY = 86400


def jl(*objs):
    return "".join(json.dumps(o, ensure_ascii=False) + "\n" for o in objs)


def claude(role, text, uuid, session="sess-claude", **extra):
    obj = {"type": role, "sessionId": session, "uuid": uuid, "cwd": "/work/proj",
           "timestamp": "2026-10-08T10:00:00Z",
           "message": {"role": role, "content": text}}
    obj.update(extra)
    return obj


class BackfillTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="sa-backfill-")
        self.home = os.path.join(self.tmp, "home")
        self.archive_dir = os.path.join(self.tmp, "brain", "archive")
        self.db = os.path.join(self.archive_dir, "sessions.sqlite")
        self.config = os.path.join(self.tmp, "collector.json")
        self.backfill = os.path.join(self.archive_dir, sa.BACKFILL_NAME)
        self.now = time.time()
        # One recent file always in scope, plus five old files (all older than the
        # 14-day window) with distinct mtimes: old-5 newest ... old-1 oldest.
        self.recent = self.put(".claude/projects/-work-proj/recent.jsonl",
                               jl(claude("user", "недавнее", "ur")))
        os.utime(self.recent, (self.now, self.now))
        self.old = {}
        for i in range(1, 6):
            path = self.put(f".claude/projects/-work-proj/old-{i}.jsonl",
                            jl(claude("user", f"старое_дело_{i}", f"u{i}")))
            self.stamp(path, 30 - i)  # -29d ... -25d
            self.old[i] = path
        self.set_config(enabled=True)

    def tearDown(self):
        for root, dirs, files in os.walk(self.tmp):
            for name in dirs + files:
                with contextlib.suppress(OSError):
                    os.chmod(os.path.join(root, name), 0o700)
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---------------------------------------------------------------- helpers

    def put(self, rel, text):
        path = os.path.join(self.home, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def stamp(self, path, days_ago):
        stamp = self.now - days_ago * DAY
        os.utime(path, (stamp, stamp))
        return path

    def set_config(self, **values):
        fd = os.open(self.config, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as handle:
            json.dump(values, handle)
        os.chmod(self.config, 0o600)

    def set_backfill(self, payload, mode=0o600):
        os.makedirs(self.archive_dir, mode=0o700, exist_ok=True)
        os.chmod(os.path.dirname(self.archive_dir), 0o700)
        fd = os.open(self.backfill, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as handle:
            handle.write(payload if isinstance(payload, str) else json.dumps(payload))
        os.chmod(self.backfill, mode)

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = sa.main(["--database", self.db, *argv])
        return code, (json.loads(out.getvalue()) if code == 0 else None), err.getvalue()

    def collect(self):
        code, data, err = self.run_cli("collect", "--home", self.home, "--config", self.config)
        self.assertEqual(code, 0, err)
        return data

    def search(self, query):
        code, data, err = self.run_cli("search", query)
        self.assertEqual(code, 0, err)
        return data["results"]

    def cursor_state(self, path):
        conn = sqlite3.connect(self.db)
        try:
            row = conn.execute(
                "SELECT c.state FROM cursors c JOIN sources s ON s.id = c.source_id"
                " WHERE s.path = ?", (os.path.realpath(path),)).fetchone()
            return row[0] if row else None
        finally:
            conn.close()

    # ---------------------------------------------------------------- tests

    def test_no_backfill_config_keeps_previous_behavior(self):
        data = self.collect()
        self.assertIsNone(data["backfill"])  # no backfill config => untouched contract
        self.assertEqual(data["clients"]["claude"]["out_of_window"], 5)
        self.assertEqual(data["clients"]["claude"]["inserted"], 1)  # only the recent file
        for i in range(1, 6):
            self.assertEqual(self.search(f"старое_дело_{i}"), [])
        self.assertEqual(self.cursor_state(self.recent), "caught_up")

    def test_two_newest_old_files_admitted(self):
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"})
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 2)
        self.assertEqual(data["backfill"]["admitted_files"], 2)
        self.assertEqual(data["backfill"]["remaining_out_of_window"], 3)
        self.assertEqual(data["backfill"]["config"]["state"], "running")
        for i in (4, 5):  # the two newest old files were taken
            self.assertEqual(len(self.search(f"старое_дело_{i}")), 1, i)
        for i in (1, 2, 3):  # the older three were not
            self.assertEqual(self.search(f"старое_дело_{i}"), [], i)
        self.assertEqual(self.cursor_state(self.old[5]), "caught_up")

    def test_watermark_gap_until_everything_newer_is_caught_up(self):
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"})
        first = self.collect()
        self.assertIsNone(first["backfill"]["watermark_mtime"])  # gap remains
        self.assertFalse(first["backfill"]["contiguous"])
        second = self.collect()
        self.assertIsNone(second["backfill"]["watermark_mtime"])  # one old file left
        self.assertEqual(second["backfill"]["remaining_out_of_window"], 1)
        third = self.collect()
        self.assertEqual(third["backfill"]["remaining_out_of_window"], 0)
        self.assertTrue(third["backfill"]["contiguous"])
        self.assertIsNotNone(third["backfill"]["watermark_mtime"])
        # The watermark never claims the whole history is captured.
        self.assertFalse(third["complete_capture_claimed"])
        self.assertEqual(third["backfill"]["admitted_files"], 1)

    def test_stopped_prevents_new_admission_but_tracked_finish(self):
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"})
        self.collect()  # admit old-5, old-4
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "stopped"})
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)
        self.assertEqual(data["backfill"]["config"]["state"], "stopped")
        self.assertIsNone(data["backfill"]["watermark_mtime"])
        self.assertEqual(data["backfill"]["remaining_out_of_window"], 3)
        # old-5 and old-4 stay tracked (still caught up); no new source appeared
        self.assertEqual(self.cursor_state(self.old[5]), "caught_up")
        self.assertEqual(self.cursor_state(self.old[4]), "caught_up")
        self.assertIsNone(self.cursor_state(self.old[3]))

    def test_zero_files_per_run_admits_nothing(self):
        self.set_backfill({"v": 1, "files_per_run": 0, "state": "running"})
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)
        self.assertEqual(data["backfill"]["remaining_out_of_window"], 5)
        self.assertEqual(data["clients"]["claude"]["inserted"], 1)

    def test_malformed_backfill_denies_backfill_only_and_reports_reason(self):
        # A fully-formed file with an unsupported version: all required keys are
        # present, so the reported reason is the version, not a missing key.
        self.set_backfill({"v": 99, "files_per_run": 2, "state": "running"})
        data = self.collect()
        self.assertEqual(data["status"], "ok")  # regular collection still ran
        self.assertEqual(data["clients"]["claude"]["inserted"], 1)
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)
        self.assertEqual(data["backfill"]["remaining_out_of_window"], 5)
        self.assertIsNone(data["backfill"]["watermark_mtime"])
        self.assertIn("version", data["backfill"]["error"])

    def test_backfill_config_validation(self):
        bad = ({"v": 1, "files_per_run": 21, "state": "running"},
               {"v": 1, "files_per_run": -1, "state": "running"},
               {"v": 1, "files_per_run": True, "state": "running"},
               {"v": 1, "files_per_run": 1, "state": "paused"},
               {"v": 1, "files_per_run": 1, "extra": 0},
               {"v": 1, "files_per_run": 1, "state": "running", "x": 1},
               # every field is required, matching the Rust reader
               {"files_per_run": 2, "state": "running"},
               {"v": 1, "state": "running"},
               {"v": 1, "files_per_run": 2},
               {},
               "{not json", [1, 2, 3])
        for payload in bad:
            with self.assertRaises(sa.ArchiveError, msg=payload):
                self.set_backfill(payload)
                sa.load_backfill(self.backfill)
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"}, mode=0o666)
        with self.assertRaises(sa.ArchiveError):
            sa.load_backfill(self.backfill)  # group writable => insecure
        os.remove(self.backfill)
        self.assertIsNone(sa.load_backfill(self.backfill))  # absent => no backfill

    def test_symlinked_backfill_config_denied(self):
        os.makedirs(self.archive_dir, mode=0o700, exist_ok=True)
        os.chmod(os.path.dirname(self.archive_dir), 0o700)
        target = os.path.join(self.tmp, "elsewhere.json")
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(json.dumps({"v": 1, "files_per_run": 2, "state": "running"}))
        os.symlink(target, self.backfill)
        with self.assertRaises(sa.ArchiveError):
            sa.load_backfill(self.backfill)
        data = self.collect()  # the symlink is not followed; the run still succeeds
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)
        self.assertIn("symlink", data["backfill"]["error"])

    def test_hermes_admitted_by_row_cursor(self):
        path = os.path.join(self.home, ".hermes", "state.db")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        conn = sqlite3.connect(path)
        conn.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
                     " content TEXT, timestamp REAL)")
        for text in ("hermes_старое_1", "hermes_старое_2"):
            conn.execute("INSERT INTO messages(session_id, role, content, timestamp)"
                         " VALUES ('h', 'user', ?, ?)", (text, self.now - 30 * DAY))
        conn.commit()
        conn.close()
        self.stamp(path, 20)  # newest old file overall (newer than the claude old files)
        self.set_backfill({"v": 1, "files_per_run": 1, "state": "running"})
        data = self.collect()
        self.assertEqual(data["clients"]["hermes"]["admitted"], 1)
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)
        self.assertEqual(len(self.search("hermes_старое_1")), 1)
        self.assertEqual(self.cursor_state(path), "caught_up")

    def test_utf8_history_is_admitted_intact(self):
        path = self.put(".claude/projects/-work-proj/old-utf8.jsonl",
                        jl(claude("user", "текст с эмодзи 🚀 и ключ_utf8", "u8")))
        self.stamp(path, 20)  # newest old file overall
        self.set_backfill({"v": 1, "files_per_run": 1, "state": "running"})
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 1)
        hits = self.search("ключ_utf8")
        self.assertEqual(len(hits), 1)
        code, rec, err = self.run_cli("read", str(hits[0]["id"]))
        self.assertEqual(code, 0, err)
        self.assertIn("🚀", rec["content"])

    def test_partial_import_resumes_on_cursor_replay(self):
        # A big old file that cannot be read in one bounded run: admission happens once,
        # the cursor resumes it, and re-runs do not duplicate stored entries.
        # Other old files are removed so the only admission on each run is this one,
        # making "admitted == 0 once it is tracked" a precise statement about the quota.
        for i in range(1, 6):
            os.remove(self.old[i])
        self.set_config(enabled=True, max_bytes_per_source=65536, max_bytes_per_run=1 << 20)
        big = [claude("user", f"строка_{i} " + "x" * 500, f"b{i:04d}") for i in range(400)]
        path = self.stamp(self.put(".claude/projects/-work-proj/old-big.jsonl", jl(*big)), 20)
        self.set_backfill({"v": 1, "files_per_run": 1, "state": "running"})
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 1)
        self.assertGreater(data["clients"]["claude"]["inserted"], 0)
        self.assertEqual(self.cursor_state(path), "pending")  # bounded, not finished
        for _ in range(30):
            data = self.collect()
            if self.cursor_state(path) == "caught_up":
                break
        self.assertEqual(self.cursor_state(path), "caught_up")
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)  # already tracked
        conn = sqlite3.connect(self.db)
        total, distinct = conn.execute(
            "SELECT count(*), count(DISTINCT evidence_sha256) FROM entries").fetchone()
        conn.close()
        self.assertEqual(total, distinct)
        self.assertEqual(len(self.search("строка_399")), 1)

    def test_global_max_files_per_run_bounded(self):
        # files_per_run is capped at 20 even when a huge backlog waits. Prove real
        # tracking for each admitted file, not just a mirrored selection count.
        for i in range(6, 30):
            path = self.put(f".claude/projects/-work-proj/old-{i}.jsonl",
                            jl(claude("user", f"старое_дело_{i}", f"u{i}")))
            self.stamp(path, 60 - i)  # -54d ... -31d, all old
        self.set_backfill({"v": 1, "files_per_run": 20, "state": "running"})
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 20)
        self.assertEqual(data["backfill"]["admitted_files"], 20)
        self.assertEqual(data["backfill"]["remaining_out_of_window"], 9)  # 29 old - 20
        self.assertLessEqual(data["backfill"]["config"]["files_per_run"], sa.BACKFILL_MAX_FILES)
        # Genuine tracking: the 20 newest old files (old-6..old-25) each have a real
        # cursor row; the 9 oldest (old-26..old-29 and old-1..old-5) have none.
        tracked = 0
        for i in list(range(6, 30)) + list(self.old):
            path = os.path.join(self.home, f".claude/projects/-work-proj/old-{i}.jsonl")
            state = self.cursor_state(path)
            if state is not None:
                tracked += 1
                self.assertEqual(state, "caught_up", i)
        self.assertEqual(tracked, 20)

    def test_coverage_not_contiguous_when_admitted_source_uncertain(self):
        # Root defect 1: a source AT the cutoff that is admitted but not yet caught must
        # not yield a "contiguous" watermark. Model it directly through the helper:
        # candidates {new (caught), old (not_started)} with the old one exactly at the
        # boundary. With no uncaught-but-unrepresented gap, the honest answer is covered
        # = [new] only, so the watermark is new's mtime, never the old one's.
        new_info = type("Info", (), {"st_mtime": 20})()
        old_info = type("Info", (), {"st_mtime": 10})()
        candidates = [("claude", "/new", new_info, None), ("claude", "/old", old_info, None)]
        states = {("claude", "/new"): "caught_up", ("claude", "/old"): "not_started"}
        bf = {"state": "running", "files_per_run": 2, "admitted": 2}
        cov = sa._backfill_coverage(bf, candidates, states, 0, 0, False)
        self.assertEqual(cov["watermark_mtime"], 20)  # only the caught level counts
        self.assertTrue(cov["contiguous"])
        # admitted-but-not-caught at the *newest* level -> nothing covered at all
        states2 = {("claude", "/new"): "pending", ("claude", "/old"): "not_started"}
        cov2 = sa._backfill_coverage(bf, candidates, states2, 0, 0, False)
        self.assertIsNone(cov2["watermark_mtime"])
        self.assertFalse(cov2["contiguous"])
        # same-mtime tie: one caught, one not -> that level is a gap
        tie_info = type("Info", (), {"st_mtime": 20})()
        candidates3 = [("claude", "/a", tie_info, None), ("claude", "/b", tie_info, None)]
        cov3 = sa._backfill_coverage(bf, candidates3,
                                     {("claude", "/a"): "caught_up", ("claude", "/b"): "pending"},
                                     0, 0, False)
        self.assertIsNone(cov3["watermark_mtime"])
        # a bounded/truncated discovery gap (root's requirement) forces null too
        cov4 = sa._backfill_coverage(bf, candidates, {("claude", "/new"): "caught_up"},
                                     0, 0, True)
        self.assertIsNone(cov4["watermark_mtime"])
        self.assertFalse(cov4["contiguous"])

    def test_admitted_but_not_caught_source_keeps_out_of_window(self):
        # Root defect 1/4 end to end: with a single source budget the second admitted
        # file is selected but never visited; it must stay out_of_window (honest) and
        # the watermark must stay null, then recover on the next run.
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"})
        self.set_config(enabled=True, max_sources_per_run=1)
        first = self.collect()
        self.assertEqual(first["clients"]["claude"]["admitted"], 1)  # only one got a cursor
        self.assertEqual(first["clients"]["claude"]["selected"], 2)  # two were chosen
        self.assertEqual(first["clients"]["claude"]["out_of_window"], 4)  # 5 old - 1
        self.assertIsNone(first["backfill"]["watermark_mtime"])
        self.assertFalse(first["backfill"]["contiguous"])
        data = first
        for _ in range(12):
            data = self.collect()
            if data["backfill"]["contiguous"]:
                break
        self.assertEqual(data["clients"]["claude"]["out_of_window"], 0)
        self.assertTrue(data["backfill"]["contiguous"])
        for i in range(1, 6):
            self.assertEqual(len(self.search(f"старое_дело_{i}")), 1, i)

    def test_backfill_config_owner_only_and_strict_version(self):
        # Root defect 2: mode 0644 and a boolean v must be refused.
        for mode in (0o644, 0o604, 0o640, 0o606, 0o666):
            self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"}, mode=mode)
            with self.assertRaises(sa.ArchiveError, msg=oct(mode)):
                sa.load_backfill(self.backfill)
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"}, mode=0o600)
        self.assertIsNotNone(sa.load_backfill(self.backfill))  # 0600 is fine
        os.chmod(self.backfill, 0o400)  # owner-only read is still acceptable
        self.assertIsNotNone(sa.load_backfill(self.backfill))
        os.chmod(self.backfill, 0o600)
        for bad_version in (True, False, 1.0, "1"):
            self.set_backfill({"v": bad_version, "files_per_run": 2, "state": "running"})
            with self.assertRaises(sa.ArchiveError, msg=repr(bad_version)):
                sa.load_backfill(self.backfill)

    def test_backfill_config_oversized_and_fifo_rejected_without_blocking(self):
        # Root defect 3: an oversized file must not be accepted by its valid prefix, and
        # a FIFO must be rejected without blocking on a writer.
        self.set_backfill({"v": 1, "files_per_run": 2, "state": "running"})
        with open(self.backfill, "a", encoding="utf-8") as handle:
            handle.write(" " * (64 << 10))  # valid prefix + >64 KiB tail
        os.chmod(self.backfill, 0o600)
        with self.assertRaises(sa.ArchiveError):
            sa.load_backfill(self.backfill)
        # FIFO: create it and confirm the read does not hang (the test would time out).
        fifo = os.path.join(self.archive_dir, "bf.fifo")
        os.mkfifo(fifo, 0o600)
        with self.assertRaises(sa.ArchiveError):
            sa.load_backfill(fifo)

    def test_gap_reported_as_not_contiguous(self):
        # A followed source that disappeared is a real gap: never represented as complete.
        self.set_backfill({"v": 1, "files_per_run": 5, "state": "running"})
        self.collect()
        os.remove(self.old[1])  # a followed source is now missing
        data = self.collect()
        self.assertGreaterEqual(data["clients"]["claude"]["missing"], 1)
        self.assertIsNone(data["backfill"]["watermark_mtime"])
        self.assertFalse(data["backfill"]["contiguous"])

    def test_admitted_source_not_re_admitted_when_window_widens(self):
        self.set_backfill({"v": 1, "files_per_run": 1, "state": "running"})
        self.collect()
        admitted_state = self.cursor_state(self.old[5])
        self.set_config(enabled=True, since_days=3650)  # now everything is in window
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["admitted"], 0)  # nothing new to admit
        self.assertEqual(self.cursor_state(self.old[5]), admitted_state)
        self.assertEqual(len(self.search("старое_дело_5")), 1)

    def test_crash_during_backfill_admission_no_loss_no_duplicates(self):
        self.set_backfill({"v": 1, "files_per_run": 5, "state": "running"})
        script = (
            "import importlib.util, os\n"
            f"spec = importlib.util.spec_from_file_location('sa', {str(MODULE_PATH)!r})\n"
            "sa = importlib.util.module_from_spec(spec); spec.loader.exec_module(sa)\n"
            "real = sa._store; n = [0]\n"
            "def dying(*a, **kw):\n"
            "    n[0] += 1\n"
            "    if n[0] == 3: os._exit(9)\n"
            "    return real(*a, **kw)\n"
            "sa._store = dying\n"
            f"sa.main(['--database', {self.db!r}, 'collect', '--home', {self.home!r},"
            f" '--config', {self.config!r}])\n")
        proc = subprocess.run([sys.executable, "-I", "-B", "-c", script], capture_output=True)
        self.assertEqual(proc.returncode, 9)
        self.collect()
        self.collect()
        conn = sqlite3.connect(self.db)
        total, distinct = conn.execute(
            "SELECT count(*), count(DISTINCT evidence_sha256) FROM entries").fetchone()
        conn.close()
        self.assertEqual(total, distinct)  # every stored row is unique
        for i in range(1, 6):
            self.assertEqual(len(self.search(f"старое_дело_{i}")), 1, i)


if __name__ == "__main__":
    unittest.main()
