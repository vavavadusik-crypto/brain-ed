"""Bounded collector: discovery, cursors, budgets, crash safety, honest states."""
import contextlib
import fcntl
import hashlib
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
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "tools" / "session_archive.py"
_spec = importlib.util.spec_from_file_location("session_archive", MODULE_PATH)
sa = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sa)


def jl(*objs):
    return "".join(json.dumps(o, ensure_ascii=False) + "\n" for o in objs)


def claude(role, text, uuid, session="sess-claude", **extra):
    obj = {"type": role, "sessionId": session, "uuid": uuid, "cwd": "/work/proj",
           "timestamp": f"2026-10-08T10:00:{len(uuid):02d}Z",
           "message": {"role": role, "content": text}}
    obj.update(extra)
    return obj


def codex_meta(session="sess-codex"):
    return {"type": "session_meta", "timestamp": "2026-10-08T09:00:00Z",
            "payload": {"id": session, "cwd": "/work/codexproj"}}


def codex(role, text, n):
    kind = "input_text" if role == "user" else "output_text"
    return {"type": "response_item", "timestamp": f"2026-10-08T09:00:{n:02d}Z",
            "payload": {"type": "message", "role": role,
                        "content": [{"type": kind, "text": text}]}}


def passthrough(obj, kinds, phase=None):
    """Current Codex wire shape: client metadata on the message (synthetic values)."""
    obj["payload"]["internal_chat_message_metadata_passthrough"] = {
        "content_item_kinds": kinds, "turn_id": "turn-x"}
    if phase:
        obj["payload"]["phase"] = phase
    return obj


class CollectorTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="sa-collect-")
        self.home = os.path.join(self.tmp, "home")
        self.archive_dir = os.path.join(self.tmp, "brain", "archive")
        self.db = os.path.join(self.archive_dir, "sessions.sqlite")
        self.config = os.path.join(self.tmp, "collector.json")
        self.claude_file = self.put(".claude/projects/-work-proj/s1.jsonl", jl(
            {"type": "system", "content": "system_secret"},
            claude("user", "ключевое_слово_claude вопрос", "u1"),
            claude("assistant", [{"type": "thinking", "thinking": "hidden_reasoning"},
                                 {"type": "text", "text": "ответ claude_answer"}], "a1"),
            claude("user", "subagent_secret", "s1x", isSidechain=True),
        ))
        self.codex_file = self.put(".codex/sessions/2026/10/08/rollout-a.jsonl", jl(
            codex_meta(),
            codex("developer", "<permissions instructions> dev_secret", 1),
            codex("user", "ключевое_слово_codex задача", 2),
            codex("assistant", "codex_answer готово", 3),
        ))
        self.hermes_file = self.make_hermes()
        self.set_config(enabled=True)

    def tearDown(self):
        for root, dirs, files in os.walk(self.tmp):
            for name in dirs + files:
                with contextlib.suppress(OSError):
                    os.chmod(os.path.join(root, name), 0o700)
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---------------------------------------------------------------- helpers

    def put(self, rel, text, mode="w"):
        path = os.path.join(self.home, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, mode, encoding="utf-8") as handle:
            handle.write(text)
        return path

    def replace_atomically(self, path, text):
        """Swap in a new file while the old one still exists.

        unlink + create may reuse the freed inode (ext4 does), which is
        indistinguishable from an in-place rewrite; a rename over a live file
        guarantees the new inode differs from the one the cursor remembers.
        """
        staging = os.path.join(self.tmp, "staging.jsonl")
        with open(staging, "w", encoding="utf-8") as handle:
            handle.write(text)
        old_inode = os.stat(path).st_ino
        os.replace(staging, path)
        self.assertNotEqual(os.stat(path).st_ino, old_inode)

    def make_hermes(self):
        path = os.path.join(self.home, ".hermes", "state.db")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        conn = sqlite3.connect(path)
        conn.executescript(
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
            " content TEXT, reasoning TEXT, timestamp REAL);")
        for row in [("h1", "user", "ключевое_слово_hermes привет", None),
                    ("h1", "assistant", "<think>hidden_think</think>hermes_answer", "reason_secret"),
                    ("h1", "tool", "tool_secret", None)]:
            conn.execute("INSERT INTO messages(session_id, role, content, reasoning, timestamp)"
                         " VALUES (?, ?, ?, ?, 1791400000.5)", row)
        conn.commit()
        conn.close()
        return path

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

    def entries(self):
        conn = sqlite3.connect(self.db)
        try:
            return conn.execute("SELECT count(*), count(DISTINCT evidence_sha256)"
                                " FROM entries").fetchone()
        finally:
            conn.close()

    def cursor(self, path):
        conn = sqlite3.connect(self.db)
        try:
            row = conn.execute(
                "SELECT c.state, c.position, c.resets, c.last_reset_reason, c.last_error"
                " FROM cursors c JOIN sources s ON s.id = c.source_id WHERE s.path = ?",
                (os.path.realpath(path),)).fetchone()
            return dict(zip(("state", "position", "resets", "reason", "error"), row))
        finally:
            conn.close()

    @staticmethod
    def sha(path):
        with open(path, "rb") as handle:
            return hashlib.sha256(handle.read()).hexdigest()

    # ---------------------------------------------------------------- tests

    def test_missing_or_disabled_config_touches_nothing(self):
        os.remove(self.config)
        self.assertEqual(self.collect()["status"], "not_configured")
        self.set_config(enabled=False)
        self.assertEqual(self.collect()["status"], "disabled")
        self.assertFalse(os.path.exists(self.archive_dir))

    def test_all_three_clients_collected_visible_text_only(self):
        before = {p: self.sha(p) for p in (self.claude_file, self.codex_file, self.hermes_file)}
        data = self.collect()
        self.assertEqual(data["status"], "ok")
        self.assertFalse(data["complete_capture_claimed"])
        self.assertEqual(data["stored_entries"], {"claude": 2, "codex": 2, "hermes": 2})
        for client in ("claude", "codex", "hermes"):
            hits = self.search(f"ключевое_слово_{client}")
            self.assertEqual([h["client"] for h in hits], [client])
            self.assertEqual(data["clients"][client]["sources_by_state"], {"caught_up": 1})
        for secret in ("system_secret", "hidden_reasoning", "subagent_secret", "dev_secret",
                       "hidden_think", "reason_secret", "tool_secret"):
            self.assertEqual(self.search(secret), [], secret)
        code, rec, _ = self.run_cli("read", str(self.search("ключевое_слово_codex")[0]["id"]))
        self.assertEqual((rec["cwd"], rec["session_id"]), ("/work/codexproj", "sess-codex"))
        code, rec, _ = self.run_cli("read", str(self.search("ключевое_слово_claude")[0]["id"]))
        self.assertEqual(rec["cwd"], "/work/proj")
        self.assertEqual(before, {p: self.sha(p) for p in before})

    def test_second_run_inserts_nothing_and_skips_unchanged_files(self):
        self.collect()
        data = self.collect()
        self.assertEqual(sum(c["inserted"] for c in data["clients"].values()), 0)
        self.assertEqual(data["clients"]["claude"]["visited"], 0)
        self.assertEqual(data["clients"]["codex"]["visited"], 0)
        self.assertEqual(self.entries(), (6, 6))

    def test_appended_record_imported_once(self):
        self.collect()
        self.put(".codex/sessions/2026/10/08/rollout-a.jsonl",
                 jl(codex("user", "добавлено_позже", 9)), mode="a")
        conn = sqlite3.connect(self.hermes_file)
        conn.execute("INSERT INTO messages(session_id, role, content, timestamp)"
                     " VALUES ('h1', 'user', 'hermes_позже', 1791400001)")
        conn.commit()
        conn.close()
        data = self.collect()
        self.assertEqual((data["clients"]["codex"]["inserted"],
                          data["clients"]["hermes"]["inserted"]), (1, 1))
        self.assertEqual(self.collect()["clients"]["codex"]["inserted"], 0)
        self.assertEqual(len(self.search("добавлено_позже")), 1)
        self.assertEqual(self.search("добавлено_позже")[0]["session_id"], "sess-codex")

    def test_partial_last_record_waits_then_imports_once(self):
        line = json.dumps(claude("user", "хвост_строки", "u9"), ensure_ascii=False)
        self.put(".claude/projects/-work-proj/s1.jsonl", line[:20], mode="a")
        data = self.collect()
        self.assertEqual(self.cursor(self.claude_file)["state"], "partial_tail")
        self.assertEqual(data["clients"]["claude"]["sources_by_state"], {"partial_tail": 1})
        self.assertEqual(self.collect()["clients"]["claude"]["visited"], 0)  # unchanged tail
        self.put(".claude/projects/-work-proj/s1.jsonl", line[20:] + "\n", mode="a")
        self.collect()
        self.assertEqual(len(self.search("хвост_строки")), 1)
        self.assertEqual(self.cursor(self.claude_file)["state"], "caught_up")

    def test_truncation_and_rotation_reset_without_duplicates(self):
        self.collect()
        with open(self.claude_file, "r+b") as handle:
            handle.truncate(10)  # shorter than the cursor
        self.collect()
        state = self.cursor(self.claude_file)
        self.assertEqual((state["resets"], state["reason"]), (1, "truncated"))
        self.assertNotEqual(state["state"], "caught_up")  # 10 bytes of half a line
        # rotation: a new file (new inode) at the same path with old + new content
        self.replace_atomically(self.codex_file, jl(
            codex_meta(), codex("user", "ключевое_слово_codex задача", 2),
            codex("assistant", "codex_answer готово", 3), codex("user", "после_ротации", 4)))
        data = self.collect()
        state = self.cursor(self.codex_file)
        self.assertEqual((state["reason"], state["state"]), ("replaced", "caught_up"))
        self.assertEqual(data["clients"]["codex"]["inserted"], 1)
        self.assertEqual(data["clients"]["codex"]["duplicates"], 2)
        # head rewritten in place with the same size class
        self.put(".codex/sessions/2026/10/08/rollout-a.jsonl", jl(
            codex_meta("sess-new"), codex("user", "переписан", 1)) + " " * 4096)
        self.collect()
        self.assertEqual(self.cursor(self.codex_file)["resets"], 2)
        self.assertEqual(len(self.search("переписан")), 1)

    def test_same_inode_rewrites_reset_by_observed_reason_without_duplicates(self):
        """In-place truncation/rewrite keeps the inode; the recorded reason is what was observed."""
        self.collect()
        inode = os.stat(self.claude_file).st_ino
        with open(self.claude_file, "r+b") as handle:  # shorter than the cursor
            handle.truncate(0)
            handle.write(jl(claude("user", "ключевое_слово_claude вопрос", "u1")).encode())
        data = self.collect()
        self.assertEqual(os.stat(self.claude_file).st_ino, inode)
        state = self.cursor(self.claude_file)
        self.assertEqual((state["resets"], state["reason"], state["state"]),
                         (1, "truncated", "caught_up"))
        self.assertEqual((data["clients"]["claude"]["inserted"],
                          data["clients"]["claude"]["duplicates"]), (0, 1))
        inode = os.stat(self.codex_file).st_ino
        with open(self.codex_file, "r+b") as handle:  # longer, different head
            handle.truncate(0)
            handle.write(jl(codex_meta(), codex("user", "ключевое_слово_codex задача", 2),
                            codex("assistant", "codex_answer готово", 3),
                            codex("user", "дописано_на_месте " + "y" * 200, 4)).encode())
        data = self.collect()
        self.assertEqual(os.stat(self.codex_file).st_ino, inode)
        state = self.cursor(self.codex_file)
        self.assertEqual((state["resets"], state["reason"], state["state"]),
                         (1, "rewritten", "caught_up"))
        self.assertEqual((data["clients"]["codex"]["inserted"],
                          data["clients"]["codex"]["duplicates"]), (1, 2))
        self.assertEqual(self.collect()["clients"]["codex"]["inserted"], 0)
        self.assertEqual(len(self.search("дописано_на_месте")), 1)
        self.assertEqual(self.entries(), (7, 7))

    def test_rotation_larger_than_budget_is_not_reported_complete(self):
        self.set_config(enabled=True, max_bytes_per_source=65536, max_bytes_per_run=1 << 20)
        self.collect()
        big = [codex_meta()] + [codex("user", f"строка_{i} " + "x" * 500, i % 60)
                                for i in range(400)]
        self.replace_atomically(self.codex_file, jl(*big))
        data = self.collect()
        state = self.cursor(self.codex_file)
        self.assertEqual(state["state"], "pending")
        self.assertEqual(data["clients"]["codex"]["sources_by_state"], {"pending": 1})
        self.assertLess(data["clients"]["codex"]["bytes_covered"],
                        data["clients"]["codex"]["bytes_in_scope"])
        for _ in range(10):
            if self.collect()["clients"]["codex"]["sources_by_state"] == {"caught_up": 1}:
                break
        self.assertEqual(self.cursor(self.codex_file)["state"], "caught_up")

    def test_crash_mid_run_then_restart_no_duplicates_no_loss(self):
        many = [claude("user", f"crash_msg_{i}", f"c{i:03d}") for i in range(50)]
        self.put(".claude/projects/-work-proj/s2.jsonl", jl(*many))
        script = (
            "import importlib.util, os, sys\n"
            f"spec = importlib.util.spec_from_file_location('sa', {str(MODULE_PATH)!r})\n"
            "sa = importlib.util.module_from_spec(spec); spec.loader.exec_module(sa)\n"
            "real = sa._store; n = [0]\n"
            "def dying(*a, **kw):\n"
            "    n[0] += 1\n"
            "    if n[0] == 30: os._exit(9)\n"
            "    return real(*a, **kw)\n"
            "sa._store = dying\n"
            f"sa.main(['--database', {self.db!r}, 'collect', '--home', {self.home!r},"
            f" '--config', {self.config!r}])\n")
        proc = subprocess.run([sys.executable, "-I", "-c", script], capture_output=True)
        self.assertEqual(proc.returncode, 9)
        self.collect()
        self.collect()
        count, distinct = self.entries()
        self.assertEqual(count, distinct)
        self.assertEqual(count, 6 + 50)
        self.assertEqual(len(self.search("crash_msg_49")), 1)

    def test_in_process_failure_rolls_back_cursor_with_entries(self):
        real = sa._store
        calls = [0]

        def failing(*args, **kwargs):
            calls[0] += 1
            if calls[0] == 2:
                raise sqlite3.OperationalError("simulated")
            return real(*args, **kwargs)
        with mock.patch.object(sa, "_store", failing):
            data = self.collect()
        errored = [c for c in ("claude", "codex", "hermes")
                   if data["clients"][c]["sources_by_state"].get("error")]
        self.assertEqual(len(errored), 1)
        self.collect()
        self.assertEqual(self.entries(), (6, 6))

    def test_round_robin_with_source_budget(self):
        for i in range(5):
            self.put(f".claude/projects/-work-p{i}/s.jsonl", jl(claude("user", f"rr_{i}", f"r{i}")))
        self.set_config(enabled=True, max_sources_per_run=2)
        visited = []
        for _ in range(5):
            data = self.collect()
            visited.append(data["visited"])
            if data["stopped_by"] is None and data["pending_before_run"] <= 2:
                break
        self.assertTrue(all(v <= 2 for v in visited))
        for i in range(5):
            self.assertEqual(len(self.search(f"rr_{i}")), 1)
        self.assertEqual(self.entries()[0], 6 + 5)

    def test_record_budget_stops_at_record_boundary(self):
        self.set_config(enabled=True, max_records_per_run=1)
        data = self.collect()
        self.assertEqual(sum(c["inserted"] for c in data["clients"].values()), 1)
        for _ in range(8):
            self.collect()
        self.assertEqual(self.entries(), (6, 6))

    def test_oversized_line_skipped_across_runs(self):
        with mock.patch.object(sa, "MAX_LINE", 200):
            self.collect()
            self.put(".claude/projects/-work-proj/s1.jsonl", "{" + "y" * 300, mode="a")
            self.collect()
            self.assertEqual(self.cursor(self.claude_file)["state"], "pending")
            self.put(".claude/projects/-work-proj/s1.jsonl", "}\n" + jl(
                claude("user", "после_длинной", "u77")), mode="a")
            self.collect()
        self.assertEqual(len(self.search("после_длинной")), 1)
        self.assertEqual(self.cursor(self.claude_file)["state"], "caught_up")

    def test_old_history_outside_window_counted_not_collected(self):
        old = self.put(".claude/projects/-old/s.jsonl", jl(claude("user", "старое_дело", "o1")))
        past = time.time() - 40 * 86400
        os.utime(old, (past, past))
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["out_of_window"], 1)
        self.assertEqual(self.search("старое_дело"), [])
        self.set_config(enabled=True, since_days=60)
        self.collect()
        self.assertEqual(len(self.search("старое_дело")), 1)

    def test_invalid_hermes_schema_and_permissions_surfaced(self):
        os.remove(self.hermes_file)
        conn = sqlite3.connect(self.hermes_file)
        conn.execute("CREATE TABLE other(x)")
        conn.commit()
        conn.close()
        os.chmod(self.claude_file, 0)
        data = self.collect()
        self.assertEqual(data["clients"]["hermes"]["sources_by_state"], {"error": 1})
        self.assertEqual(data["clients"]["claude"]["sources_by_state"], {"error": 1})
        self.assertIn("lacks messages", self.cursor(self.hermes_file)["error"])
        self.assertEqual(self.cursor(self.claude_file)["error"], "PermissionError")
        self.assertEqual(data["stored_entries"], {"codex": 2})

    def test_missing_clients_reported_absent(self):
        shutil.rmtree(os.path.join(self.home, ".codex"))
        os.remove(self.hermes_file)
        data = self.collect()
        self.assertEqual(data["clients"]["codex"]["root"], "absent")
        self.assertEqual(data["clients"]["hermes"]["root"], "absent")
        self.assertEqual(data["clients"]["claude"]["root"], "present")

    def test_symlinked_sources_ignored(self):
        target = os.path.join(self.tmp, "elsewhere.jsonl")
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(jl(claude("user", "через_ссылку", "l1")))
        os.symlink(target, os.path.join(self.home, ".claude/projects/-work-proj/link.jsonl"))
        self.collect()
        self.assertEqual(self.search("через_ссылку"), [])

    def test_config_validation(self):
        for bad in ({"enabled": "yes"}, {"enabled": True, "max_seconds": 0},
                    {"enabled": True, "surprise": 1}, {"enabled": True, "since_days": True}):
            self.set_config(**bad)
            code, _, err = self.run_cli("collect", "--home", self.home, "--config", self.config)
            self.assertEqual(code, 2, bad)
            self.assertIn("collector config", err)
        self.set_config(enabled=True)
        os.chmod(self.config, 0o666)
        code, _, err = self.run_cli("collect", "--home", self.home, "--config", self.config)
        self.assertEqual(code, 2)
        self.assertFalse(os.path.exists(self.db))

    def test_invalid_client_selection_fails_before_any_discovery(self):
        for bad in ([], ["cursor"], ["codex", "codex"], "codex", [1], None):
            self.set_config(enabled=True, clients=bad)
            with mock.patch.object(sa, "_discover", side_effect=AssertionError("discovered")):
                code, _, err = self.run_cli("collect", "--home", self.home,
                                            "--config", self.config)
            self.assertEqual(code, 2, bad)
            self.assertIn("collector config clients", err)
            self.assertFalse(os.path.exists(self.db))

    def test_legacy_config_without_clients_keeps_all_three(self):
        data = self.collect()
        self.assertEqual((data["clients_selected"], data["client_selection"]),
                         (["claude", "codex", "hermes"], "legacy_all"))
        self.assertEqual(data["stored_entries"], {"claude": 2, "codex": 2, "hermes": 2})

    def test_explicit_client_selection_reads_only_selected_sources(self):
        self.set_config(enabled=True, clients=["hermes", "codex"])
        opened = []
        real_open = os.open

        def spy(path, *args, **kwargs):
            opened.append(str(path))
            return real_open(path, *args, **kwargs)
        with mock.patch.object(sa.os, "open", side_effect=spy):
            data = self.collect()
        self.assertEqual((data["clients_selected"], data["client_selection"]),
                         (["codex", "hermes"], "explicit"))
        self.assertEqual(data["stored_entries"], {"codex": 2, "hermes": 2})
        self.assertEqual(data["clients"]["claude"]["root"], "not_selected")
        self.assertEqual(data["clients"]["claude"]["discovered"], 0)
        self.assertNotIn(self.claude_file, opened)
        self.assertEqual(self.search("ключевое_слово_claude"), [])
        # deselecting a followed client stops reading it without calling it missing
        self.set_config(enabled=True, clients=["claude"])
        self.collect()
        self.set_config(enabled=True, clients=["codex"])
        self.put(".claude/projects/-work-proj/s1.jsonl",
                 jl(claude("user", "после_отказа", "u7")), mode="a")
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["missing"], 0)
        self.assertEqual(data["clients"]["claude"]["visited"], 0)
        self.assertEqual(self.search("после_отказа"), [])

    def test_concurrent_run_reports_busy(self):
        os.makedirs(self.archive_dir, mode=0o700)
        os.chmod(os.path.dirname(self.archive_dir), 0o700)
        fd = os.open(os.path.join(self.archive_dir, "collector.lock"), os.O_RDWR | os.O_CREAT, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            self.assertEqual(self.collect()["status"], "busy")
        finally:
            os.close(fd)
        self.assertEqual(self.collect()["status"], "ok")

    def test_schema2_archive_migrates_and_keeps_entries(self):
        os.makedirs(self.archive_dir, mode=0o700)
        os.chmod(os.path.dirname(self.archive_dir), 0o700)
        code, _, err = self.run_cli("import", "--client", "claude", "--source", self.claude_file)
        self.assertEqual(code, 0, err)
        conn = sqlite3.connect(self.db)  # rebuild it as a schema-2 archive
        conn.executescript("DROP TABLE cursors; DROP TABLE collector_runs;"
                           " DROP INDEX entries_evidence; DROP INDEX entries_session;"
                           " ALTER TABLE entries DROP COLUMN cwd;"
                           " UPDATE archive_meta SET value = 'brain_session_archive:2';")
        conn.close()
        self.assertEqual(self.run_cli("status")[1]["schema"], "brain_session_archive:2")
        self.collect()
        self.assertEqual(self.run_cli("status")[1]["schema"], "brain_session_archive:3")
        self.assertEqual(self.entries(), (6, 6))

    def test_identical_messages_at_different_offsets_are_both_kept(self):
        same = codex("user", "одинаково_дважды", 5)
        self.put(".codex/sessions/2026/10/08/rollout-a.jsonl", jl(same, same), mode="a")
        self.collect()
        self.assertEqual(len(self.search("одинаково_дважды")), 2)
        self.assertEqual(self.collect()["clients"]["codex"]["inserted"], 0)

    def test_empty_hermes_table_is_caught_up_not_an_error(self):
        conn = sqlite3.connect(self.hermes_file)
        conn.execute("DELETE FROM messages")
        conn.commit()
        conn.close()
        data = self.collect()
        self.assertEqual(data["clients"]["hermes"]["sources_by_state"], {"caught_up": 1})

    def test_budget_exhausted_on_last_visit_is_reported(self):
        self.set_config(enabled=True, max_records_per_run=1)
        os.remove(self.hermes_file)
        shutil.rmtree(os.path.join(self.home, ".codex"))
        self.assertEqual(self.collect()["stopped_by"], "record_budget")

    def test_codex_tool_traffic_and_injected_context_skipped_quotes_kept(self):
        # Shapes of a session Codex imported from another agent plus the current wire
        # format; every text here is synthetic.
        self.put(".codex/sessions/2026/10/08/rollout-ext.jsonl", jl(
            codex_meta("sess-ext"),
            codex("assistant", "[external_agent_tool_result]\nwrapped_tool_output exit 0", 1),
            codex("assistant", "  [external_agent_tool_call]\nwrapped_tool_call", 2),
            codex("assistant", "ordinary_visible_reply: run the tests next", 3),
            codex("user", "what does [external_agent_tool_result] mean user_quote", 4),
            codex("user", "[external_agent_tool_result] pasted_by_user", 5),
            codex("assistant", "the marker [external_agent_tool_result] inline_prose", 6),
            passthrough(codex("user", "agents_injected_text", 7),
                        ["agents_md.instructions", "environments.environment_context"]),
            passthrough(codex("user", "typed_user_text", 8), ["user.text"]),
            passthrough(codex("assistant", "commentary_visible", 9), ["unknown"], "commentary"),
        ))
        self.collect()
        for gone in ("wrapped_tool_output", "wrapped_tool_call", "agents_injected_text"):
            self.assertEqual(self.search(gone), [], gone)
        for kept, role in (("ordinary_visible_reply", "assistant"), ("user_quote", "user"),
                           ("pasted_by_user", "user"), ("inline_prose", "assistant"),
                           ("typed_user_text", "user"), ("commentary_visible", "assistant")):
            self.assertEqual([r["role"] for r in self.search(kept)], [role], kept)
        self.assertEqual(self.run_cli("status")[1]["quarantined"], {})

    def test_previously_stored_tool_traffic_is_quarantined_reversibly(self):
        rel = ".codex/sessions/2026/10/08/rollout-old1.jsonl"
        old = [codex_meta("sess-old1"),
               codex("assistant", "[external_agent_tool_result]\nold_wrapped_one", 1),
               codex("assistant", "old_visible_one", 2),
               codex("user", "quoting [external_agent_tool_result] old_user_quote", 3)]
        kept_path = self.put(rel, jl(*old))
        gone_path = self.put(".codex/sessions/2026/10/08/rollout-old2.jsonl", jl(
            codex_meta("sess-old2"),
            codex("assistant", "[external_agent_tool_result]\nold_wrapped_two", 1)))
        # What the previous filter stored: markers unknown, no quarantine yet.
        with mock.patch.object(sa, "CODEX_EXTERNAL_TOOL_MARKERS", ()):
            self.collect()
        self.assertEqual(len(self.search("old_wrapped_one")), 1)
        conn = sqlite3.connect(self.db)
        conn.execute("DROP TABLE quarantine")
        conn.execute("DELETE FROM archive_meta WHERE key = 'quarantine_rule'")
        conn.commit()
        conn.close()
        os.rename(gone_path, os.path.join(self.tmp, "moved-away.jsonl"))  # source gone
        stored = self.entries()[0]
        self.collect()
        self.assertEqual(self.search("old_wrapped_one"), [])
        self.assertEqual(self.search("old_wrapped_two"), [])
        self.assertEqual(len(self.search("old_visible_one")), 1)
        self.assertEqual(len(self.search("old_user_quote")), 1)
        self.assertEqual(self.entries()[0], stored)  # nothing deleted
        conn = sqlite3.connect(self.db)
        rows = {json.loads(e)["basis"]: (i, r) for i, r, e in conn.execute(
            "SELECT entry_id, reason, evidence FROM quarantine")}
        conn.close()
        self.assertEqual({b: r for b, (_, r) in rows.items()},
                         {"source_line": "external_agent_tool_traffic",
                          "stored_text_marker": "external_agent_tool_traffic"})
        status = self.run_cli("status")[1]
        self.assertEqual(status["quarantined"], {"external_agent_tool_traffic": 2})
        self.assertEqual(status["entries"], stored - 2)
        hidden = rows["source_line"][0]
        self.assertEqual(self.run_cli("read", str(hidden))[0], 2)
        # Appends, a rewrite reset and repeated runs never bring the row back.
        self.put(rel, jl(codex("assistant", "old_visible_after", 4)), "a")
        self.collect()
        self.put(rel, jl(*old[:2]))
        self.collect()
        self.assertEqual(self.entries()[0], stored + 1)
        self.assertEqual(self.search("old_wrapped_one"), [])
        listed = self.run_cli("quarantine", "--list")[1]
        self.assertEqual(sorted(listed["entries"][0]), ["id", "quarantined_at", "reason", "rule"])
        # Release is the undo: the original row is searchable again and stays so.
        self.assertEqual(self.run_cli("quarantine", "--release", str(hidden))[0], 0)
        self.collect()
        self.assertEqual([r["id"] for r in self.search("old_wrapped_one")], [hidden])
        self.assertEqual(self.run_cli("quarantine", "--release", str(hidden))[0], 2)

    def test_run_log_bounded_and_counts_only(self):
        with mock.patch.object(sa, "RUNS_KEPT", 3):
            for _ in range(5):
                self.collect()
        conn = sqlite3.connect(self.db)
        rows = conn.execute("SELECT summary FROM collector_runs").fetchall()
        conn.close()
        self.assertLessEqual(len(rows), 4)
        self.assertNotIn("ключевое_слово", "".join(r[0] for r in rows))


if __name__ == "__main__":
    unittest.main()
