"""Hermes journal capture: tool calls, tool results, hidden reasoning, upgrades.

Synthetic temp-dir fixtures only; no user archive or model I/O.
"""
import contextlib
import importlib.util
import io
import json
import os
import shutil
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "tools" / "session_archive.py"
_spec = importlib.util.spec_from_file_location("session_archive", MODULE_PATH)
sa = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sa)


class HermesJournalTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="sa-hermes-journal-")
        self.home = os.path.join(self.tmp, "home")
        self.archive_dir = os.path.join(self.tmp, "brain", "archive")
        self.db = os.path.join(self.archive_dir, "sessions.sqlite")
        self.config = os.path.join(self.tmp, "collector.json")
        self.hermes_file = os.path.join(self.home, ".hermes", "state.db")
        os.makedirs(os.path.dirname(self.hermes_file), exist_ok=True)
        self.make_hermes()
        self.set_config(enabled=True)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---------------------------------------------------------------- helpers

    def make_hermes(self, path=None):
        path = path or self.hermes_file
        os.makedirs(os.path.dirname(path), exist_ok=True)
        conn = sqlite3.connect(path)
        conn.executescript(
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
            " content TEXT, reasoning TEXT, tool_calls TEXT, tool_name TEXT,"
            " tool_call_id TEXT, timestamp REAL);")
        conn.commit()
        conn.close()

    def make_profile(self, name):
        path = os.path.join(self.home, ".hermes", "profiles", name, "state.db")
        self.make_hermes(path)
        return path

    def add_message(self, role, content=None, reasoning=None, tool_calls=None,
                    tool_name=None, session="s1", path=None, ts=1791400000.5):
        conn = sqlite3.connect(path or self.hermes_file)
        conn.execute(
            "INSERT INTO messages(session_id, role, content, reasoning, tool_calls,"
            " tool_name, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (session, role, content, reasoning,
             tool_calls if tool_calls is None or isinstance(tool_calls, str)
             else json.dumps(tool_calls),
             tool_name, ts))
        conn.commit()
        conn.close()

    def set_config(self, **values):
        fd = os.open(self.config, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as handle:
            json.dump(values, handle)
        os.chmod(self.config, 0o600)

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

    def rows(self, client="hermes"):
        """(role, locator, session_id, content) of every stored row of one client."""
        conn = sqlite3.connect(self.db)
        try:
            return conn.execute(
                "SELECT e.role, e.locator, e.session_id, e.content FROM entries e"
                " JOIN sources s ON s.id = e.source_id WHERE s.client = ?", (client,)).fetchall()
        finally:
            conn.close()

    def cursor(self, path):
        conn = sqlite3.connect(self.db)
        try:
            row = conn.execute(
                "SELECT c.resets, c.last_reset_reason FROM cursors c"
                " JOIN sources s ON s.id = c.source_id WHERE s.path = ?",
                (os.path.realpath(path),)).fetchone()
            return row
        finally:
            conn.close()

    # ---------------------------------------------------------------- tests

    def test_assistant_tool_calls_become_one_row_per_call(self):
        self.add_message("user", "какая погода в каире")
        self.add_message("assistant", tool_calls=[
            {"id": "c1", "type": "function",
             "function": {"name": "get_weather", "arguments": {"city": "Cairo"}}},
            {"id": "c2", "type": "function",
             "function": {"name": "lookup", "arguments": {"q": "rain"}}},
        ])
        self.collect()
        calls = sorted(row for row in self.rows() if row[0] == "tool_call")
        self.assertEqual([row[3] for row in calls],
                         ['get_weather {"city": "Cairo"}', 'lookup {"q": "rain"}'])
        # Locator scheme: the message keeps row:<id>, each call appends its own suffix.
        self.assertEqual([row[1] for row in calls], ["row:2#tool_call:c1", "row:2#tool_call:c2"])
        self.assertEqual([row[2] for row in calls], ["s1", "s1"])
        message = [row for row in self.rows() if row[0] == "user"]
        self.assertEqual([row[1] for row in message], ["row:1"])
        # A text-less assistant row yields no conversation message, only the calls.
        self.assertEqual([row[0] for row in self.rows()], ["user", "tool_call", "tool_call"])

    def test_tool_rows_become_tool_result_naming_the_tool(self):
        self.add_message("tool", "exit 0 подробность", tool_name="shell")
        self.add_message("tool", "безымянный результат")
        self.collect()
        results = {row[3] for row in self.rows() if row[0] == "tool_result"}
        self.assertEqual(results, {"shell: exit 0 подробность", "tool: безымянный результат"})
        self.assertEqual({row[1] for row in self.rows() if row[0] == "tool_result"},
                         {"row:1#tool_result", "row:2#tool_result"})
        self.assertEqual([hit["role"] for hit in self.search("подробность")], ["tool_result"])

    def test_reasoning_column_and_think_blocks_stay_hidden(self):
        self.add_message("assistant", "<think>hidden_plan</think>visible_answer",
                         reasoning="reason_secret_text")
        self.collect()
        self.assertEqual([hit["role"] for hit in self.search("visible_answer")], ["assistant"])
        for hidden in ("hidden_plan", "reason_secret_text"):
            self.assertEqual(self.search(hidden), [], hidden)
        # Hermes never contributes a reasoning row: visibility cannot be told.
        self.assertEqual([row[0] for row in self.rows()], ["assistant"])

    def test_journal_rows_are_masked_and_bounded(self):
        self.add_message("tool", "password: SuperSecretValue12345", tool_name="vault")
        self.add_message("tool", "длинный " + "x" * 3000, tool_name="log")
        self.collect()
        stored = {row[3] for row in self.rows() if row[0] == "tool_result"}
        self.assertIn("vault: password: [REDACTED]", stored)
        self.assertNotIn("SuperSecretValue12345", "".join(row[3] for row in self.rows()))
        long_row = next(row[3] for row in self.rows() if row[3].startswith("log:"))
        self.assertRegex(long_row[-80:], r"…\[\d+ more characters not archived\]$")

    def test_capture_upgrade_rereads_hermes_and_profiles_without_duplicates(self):
        sol = self.make_profile("sol")
        self.add_message("user", "ключевой вопрос")
        self.add_message("assistant", "видимый ответ")
        self.add_message("tool", "tool_secret_раньше", tool_name="shell")
        self.add_message("tool", "профильный_результат", tool_name="shell",
                         session="h1", path=sol)
        # An archive written by the previous revision: journal ignored, rev journal:1.
        with mock.patch.object(sa, "CAPTURE_REV", "journal:1"), \
                mock.patch.object(sa, "_hermes_journal", return_value=[]):
            first = self.collect()
        # Only the two top-level messages; the tool rows were hidden then.
        self.assertEqual(first["stored_entries"]["hermes"], 2)
        self.assertEqual(self.search("tool_secret_раньше"), [])
        self.assertEqual(self.search("профильный_результат"), [])
        # The upgrade run: cursors reset, history re-read once, journal joins.
        upgraded = self.collect()
        self.assertEqual(self.cursor(self.hermes_file)[0], 1)
        self.assertEqual(self.cursor(self.hermes_file)[1], "capture_upgrade")
        self.assertEqual(self.cursor(sol)[1], "capture_upgrade")
        for marker, role, session in [("tool_secret_раньше", "tool_result", "s1"),
                                      ("профильный_результат", "tool_result", "profiles/sol/h1")]:
            hits = self.search(marker)
            self.assertEqual([(hit["role"], hit["session_id"]) for hit in hits],
                             [(role, session)], marker)
        # No duplicates: every stored row is distinct evidence, and a third
        # run inserts nothing. Four rows: two messages and two tool results.
        conn = sqlite3.connect(self.db)
        total, distinct = conn.execute(
            "SELECT count(*), count(DISTINCT evidence_sha256) FROM entries").fetchone()
        conn.close()
        self.assertEqual(total, distinct)
        self.assertEqual(total, 4)
        self.assertEqual(self.collect()["clients"]["hermes"]["inserted"], 0)
        self.assertEqual(self.cursor(self.hermes_file)[0], 1)


if __name__ == "__main__":
    unittest.main()
