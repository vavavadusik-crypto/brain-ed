import contextlib
import hashlib
import importlib.util
import io
import json
import os
import shutil
import sqlite3
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "tools" / "session_archive.py"
_spec = importlib.util.spec_from_file_location("session_archive", MODULE_PATH)
sa = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sa)

RU = "Привет, проверь функцию parse_config_v2 в модуле brain_core::sync — ошибка ёжик"


def jl(*objs):
    return "".join(json.dumps(o, ensure_ascii=False) + "\n" for o in objs)


def claude_line(role, content, uuid, **extra):
    obj = {"type": role, "sessionId": "sess-c", "uuid": uuid,
           "timestamp": "2026-10-07T10:00:00Z", "message": {"role": role, "content": content}}
    obj.update(extra)
    return obj


class SessionArchiveTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="sa-test-")
        self.db = os.path.join(self.tmp, "archive", "sessions.db")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_cli(self, *argv, db=None):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = sa.main(["--database", db or self.db, *argv])
        data = json.loads(out.getvalue()) if code == 0 else None
        return code, data, out.getvalue() + err.getvalue()

    def write(self, name, text):
        path = os.path.join(self.tmp, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def imp(self, client, source):
        code, data, text = self.run_cli("import", "--client", client, "--source", source)
        self.assertEqual(code, 0, text)
        return data

    def search(self, query, limit=10):
        code, data, text = self.run_cli("search", query, "--limit", str(limit))
        self.assertEqual(code, 0, text)
        return data["results"]

    def claude_fixture(self):
        return self.write("claude.jsonl", jl(
            {"type": "system", "content": "system prompt secret"},
            claude_line("user", RU, "u1"),
            claude_line("assistant", [
                {"type": "thinking", "thinking": "hidden_reasoning_token"},
                {"type": "text", "text": "Исправил `parse_config_v2`: fn sync() -> Result<(), Error>"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "tool_payload_x"}},
            ], "a1"),
            claude_line("user", [{"type": "tool_result", "tool_use_id": "t1",
                                  "content": "tool_output_secret"}], "u2"),
            claude_line("user", "meta caveat", "u3", isMeta=True),
        ))

    # ------------------------------------------------------------ clients

    def test_claude_import_and_search(self):
        data = self.imp("claude", self.claude_fixture())
        # 2 conversation rows + the thinking block, the tool call and its result (journal).
        self.assertEqual(data["inserted"], 5)
        self.assertFalse(data["incremental"])
        self.assertFalse(data["complete_capture_claimed"])
        self.assertEqual(len(self.search("parse_config_v2")), 2)
        self.assertEqual(len(self.search("ёжик")), 1)
        self.assertEqual(len(self.search("brain_core")), 1)
        for hidden in ("secret", "caveat"):
            self.assertEqual(self.search(hidden), [], hidden)
        for token, role in (("hidden_reasoning_token", "reasoning"), ("tool_payload_x", "tool_call"),
                            ("tool_output_secret", "tool_result")):
            self.assertEqual([h["role"] for h in self.search(token)], [role], token)
        hit = self.search("Привет")[0]
        self.assertEqual((hit["client"], hit["role"], hit["session_id"]),
                         ("claude", "user", "sess-c"))
        self.assertLessEqual(len(hit["excerpt"]), sa.EXCERPT_MAX)

    def test_codex_event_msg_duplicate_excluded(self):
        source = self.write("codex.jsonl", jl(
            {"type": "session_meta", "payload": {"id": "sess-x"}},
            {"type": "response_item", "payload": {"type": "message", "role": "developer",
             "content": [{"type": "input_text", "text": "developer_rules_text"}]}},
            {"type": "response_item", "payload": {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "<environment_context>cwd envsecret"}]}},
            {"type": "response_item", "timestamp": "t1", "payload": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Собери crate brain_sync уникальноеслово"}]}},
            {"type": "event_msg", "payload": {"type": "user_message",
             "message": "Собери crate brain_sync уникальноеслово"}},
            {"type": "response_item", "payload": {"type": "reasoning",
             "summary": [{"type": "summary_text", "text": "reasoning_secret"}]}},
            {"type": "response_item", "payload": {"type": "function_call_output",
             "output": "tool_output_secret"}},
            {"type": "response_item", "payload": {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "Готово: cargo test passed"}]}},
            {"type": "event_msg", "payload": {"type": "agent_message",
             "message": "Готово: cargo test passed"}},
        ))
        data = self.imp("codex", source)
        # 2 conversation rows + the visible reasoning summary and the tool output (journal).
        self.assertEqual(data["inserted"], 4)
        self.assertEqual(len(self.search("уникальноеслово")), 1)
        self.assertEqual(self.search("cargo")[0]["session_id"], "sess-x")
        for hidden in ("developer_rules_text", "envsecret"):
            self.assertEqual(self.search(hidden), [], hidden)
        for token, role in (("reasoning_secret", "reasoning"), ("tool_output_secret", "tool_result")):
            self.assertEqual([h["role"] for h in self.search(token)], [role], token)
        _, status, _ = self.run_cli("status")
        self.assertEqual(status["by_client"], {"codex": 4})

    def test_codex_hidden_channels_excluded(self):
        def msg(text, **tags):
            return {"type": "response_item", "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": text}], **tags}}
        source = self.write("codex.jsonl", jl(
            {"type": "session_meta", "payload": {"id": "sess-x"}},
            msg("analysis_channel_secret", channel="analysis"),
            msg("analysis_phase_secret", phase="analysis"),
            msg("unknown_channel_secret", channel="scratchpad"),
            msg("list_channel_secret", channel=["final"]),
            msg("visible_final_word", channel="final"),
            msg("visible_commentary_word", phase="commentary"),
            msg("visible_answer_word", phase="final_answer"),
        ))
        data = self.imp("codex", source)
        self.assertEqual(data["inserted"], 3)
        self.assertEqual(data["skipped"], 5)  # four hidden messages + session_meta
        for hidden in ("analysis_channel_secret", "analysis_phase_secret",
                       "unknown_channel_secret", "list_channel_secret"):
            self.assertEqual(self.search(hidden), [], hidden)
        for visible in ("visible_final_word", "visible_commentary_word", "visible_answer_word"):
            self.assertEqual(len(self.search(visible)), 1, visible)

    def make_hermes(self, with_ts=True):
        path = os.path.join(self.tmp, "state.db")
        conn = sqlite3.connect(path)
        ts = ", timestamp REAL" if with_ts else ""
        conn.executescript(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, title TEXT);"
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
            f" content TEXT, tool_calls TEXT, tool_call_id TEXT{ts});"
            "INSERT INTO sessions VALUES ('h1', 'title');")
        rows = [("h1", "system", "system_prompt_secret", None, None),
                ("h1", "user", "Запусти hermes_agent.run() пожалуйста", None, None),
                ("h1", "assistant", "", '[{"name": "terminal"}]', None),
                ("h1", "tool", "tool_output_secret", None, "call1"),
                ("h1", "assistant", "Сделано, hermes_agent отработал", None, None)]
        for row in rows:
            if with_ts:
                conn.execute("INSERT INTO messages(session_id, role, content, tool_calls,"
                             " tool_call_id, timestamp) VALUES (?, ?, ?, ?, ?, 1760000000.5)", row)
            else:
                conn.execute("INSERT INTO messages(session_id, role, content, tool_calls,"
                             " tool_call_id) VALUES (?, ?, ?, ?, ?)", row)
        conn.commit()
        conn.close()
        return path

    def file_sha(self, path):
        with open(path, "rb") as handle:
            return hashlib.sha256(handle.read()).hexdigest()

    def test_hermes_import_readonly_and_versions(self):
        source = self.make_hermes()
        before = self.file_sha(source)
        data = self.imp("hermes", source)
        # The assistant tool-call row and the tool result row are journal now.
        self.assertEqual(data["inserted"], 4)
        self.assertEqual(self.file_sha(source), before)
        self.assertEqual(len(self.search("hermes_agent")), 2)
        self.assertEqual([hit["role"] for hit in self.search("tool_output_secret")],
                         ["tool_result"])
        self.assertEqual(self.search("system_prompt_secret"), [])
        hit = self.search("Запусти")[0]
        _, rec, _ = self.run_cli("read", str(hit["id"]))
        self.assertEqual(rec["locator"], "row:2")
        self.assertEqual(rec["timestamp"], "1760000000.5")
        # Edited payload becomes a new version; earlier evidence is kept.
        conn = sqlite3.connect(source)
        conn.execute("UPDATE messages SET content = 'Запусти hermes_agent.run() снова' WHERE id = 2")
        conn.commit()
        conn.close()
        self.assertEqual(self.imp("hermes", source)["inserted"], 1)
        self.assertEqual(len(self.search("Запусти")), 2)

    def test_hermes_hidden_rows_and_inline_reasoning_excluded(self):
        path = os.path.join(self.tmp, "state.db")
        conn = sqlite3.connect(path)
        conn.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
                     " content TEXT, reasoning TEXT, display_kind TEXT)")
        conn.executemany(
            "INSERT INTO messages(session_id, role, content, reasoning, display_kind)"
            " VALUES ('h1', ?, ?, ?, ?)",
            [("assistant", "hidden_row_secret", None, "hidden"),
             ("assistant", "<think>inline_think_secret</think>\nvisible_after_think", None, None),
             ("assistant", "  <think>unclosed_think_secret", None, None),
             ("assistant", "plain_visible_answer", "column_reasoning_secret", None),
             ("user", "<think>user_typed_literal</think>", None, None)])
        conn.commit()
        conn.close()
        data = self.imp("hermes", path)
        self.assertEqual(data["inserted"], 3)
        self.assertEqual(data["skipped"], 2)
        for hidden in ("hidden_row_secret", "inline_think_secret", "unclosed_think_secret",
                       "column_reasoning_secret"):
            self.assertEqual(self.search(hidden), [], hidden)
        for visible in ("visible_after_think", "plain_visible_answer", "user_typed_literal"):
            self.assertEqual(len(self.search(visible)), 1, visible)

    def assertRefused(self, source, reason):
        code, _, text = self.run_cli("import", "--client", "hermes", "--source", source)
        self.assertNotEqual(code, 0, text)
        self.assertIn(reason, text)

    def test_hermes_unsafe_source_refused_unchanged(self):
        source = self.make_hermes()
        link = os.path.join(self.tmp, "link.db")
        os.symlink(source, link)
        self.assertRefused(link, "symlinked hermes")
        for mode in (0o666, 0o664, 0o646):
            os.chmod(source, mode)
            digest = self.file_sha(source)
            self.assertRefused(source, "not group/other writable")
            self.assertEqual(stat.S_IMODE(os.stat(source).st_mode), mode, "never chmod-ed")
            self.assertEqual(self.file_sha(source), digest, "source unchanged")
        os.chmod(source, 0o644)
        wal = source + "-wal"
        with open(wal, "wb"):
            pass
        os.chmod(wal, 0o666)
        self.assertRefused(source, "not group/other writable")
        os.remove(wal)
        for name in ("messages", "MESSAGES", "Messages"):
            with self.subTest(view_name=name):
                view = os.path.join(self.tmp, "view-" + name + ".db")
                conn = sqlite3.connect(view)
                conn.executescript("CREATE TABLE raw(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
                                   " content TEXT); CREATE VIEW " + name + " AS SELECT * FROM raw;")
                conn.close()
                digest = self.file_sha(view)
                self.assertRefused(view, "not a view")
                self.assertEqual(self.file_sha(view), digest, "rejected source unchanged")

    def test_hermes_ordinary_0644_source_accepted_unchanged(self):
        source = self.make_hermes()
        os.chmod(source, 0o644)
        digest = self.file_sha(source)
        self.assertEqual(self.imp("hermes", source)["inserted"], 4)
        self.assertEqual(stat.S_IMODE(os.stat(source).st_mode), 0o644)
        self.assertEqual(self.file_sha(source), digest)
        self.assertEqual(stat.S_IMODE(os.stat(self.db).st_mode), 0o600, "archive stays private")

    def test_hermes_reads_beside_a_live_writer(self):
        source = self.make_hermes()
        writer = sqlite3.connect(source, isolation_level=None)
        try:
            writer.execute("BEGIN IMMEDIATE")
            writer.execute("INSERT INTO messages(session_id, role, content) VALUES"
                           " ('h1', 'user', 'uncommitted_hermes_text')")
            self.assertEqual(self.imp("hermes", source)["inserted"], 4)
            self.assertEqual(self.search("uncommitted_hermes_text"), [])
        finally:
            writer.execute("ROLLBACK")
            writer.close()

    def test_hermes_without_optional_timestamp(self):
        self.assertEqual(self.imp("hermes", self.make_hermes(with_ts=False))["inserted"], 4)

    # ------------------------------------------------------------ replay

    def test_repeat_import_idempotent(self):
        source = self.claude_fixture()
        self.imp("claude", source)
        again = self.imp("claude", source)
        # Journal rows (thinking, tool call, tool result) are as idempotent as conversation.
        self.assertEqual((again["inserted"], again["duplicates"]), (0, 5))
        _, status, _ = self.run_cli("status")
        self.assertEqual(status["entries"], 5)

    def test_claude_edit_same_uuid_kept_as_version(self):
        source = self.write("c.jsonl", jl(claude_line("user", "версия_один", "u1")))
        self.imp("claude", source)
        self.write("c.jsonl", jl(claude_line("user", "версия_два", "u1")))
        self.assertEqual(self.imp("claude", source)["inserted"], 1)
        self.assertEqual(len(self.search("версия_один")), 1)
        self.assertEqual(len(self.search("версия_два")), 1)

    def test_partial_final_line_retried(self):
        full = json.dumps(claude_line("assistant", "хвостовая_строка", "a9"), ensure_ascii=False)
        source = self.write("p.jsonl", jl(claude_line("user", "первая_строка", "u1")) + full[:20])
        data = self.imp("claude", source)
        self.assertEqual((data["inserted"], data["partial_tail"]), (1, 1))
        self.assertTrue(data["source_bounded_or_partial"])
        with open(source, "a", encoding="utf-8") as handle:
            handle.write(full[20:] + "\n")
        data = self.imp("claude", source)
        self.assertEqual((data["inserted"], data["partial_tail"]), (1, 0))
        self.assertEqual(len(self.search("хвостовая_строка")), 1)

    def test_malformed_json_counted_not_echoed(self):
        source = self.write("m.jsonl", '{"broken": "malformed_secret_text\n[1, 2]\n'
                            '{"type": "user", "message": {"role": "user", "content": "\\ud800"}}\n'
                            + jl(claude_line("user", "valid_message", "u1")))
        code, data, text = self.run_cli("import", "--client", "claude", "--source", source)
        self.assertEqual(code, 0, text)
        self.assertEqual((data["malformed"], data["inserted"]), (3, 1))
        self.assertNotIn("malformed_secret_text", text)

    def test_malformed_marks_partial_but_filtered_does_not(self):
        data = self.imp("claude", self.claude_fixture())
        self.assertGreater(data["skipped"], 0)
        self.assertFalse(data["source_bounded_or_partial"])
        source = self.write("bad.jsonl", "{not json\n" + jl(claude_line("user", "ok", "u1")))
        data = self.imp("claude", source)
        self.assertEqual((data["malformed"], data["inserted"]), (1, 1))
        self.assertTrue(data["source_bounded_or_partial"])

    def test_rewritten_source_same_offset_same_text_new_identity(self):
        def line(session, role):
            return jl({"type": role, "sessionId": session, "timestamp": "t1",
                       "message": {"role": role, "content": "одинаковый_текст"}})
        source = self.write("rw.jsonl", line("sess-a", "user"))
        self.assertEqual(self.imp("claude", source)["inserted"], 1)
        self.assertEqual(self.imp("claude", source)["duplicates"], 1)
        self.write("rw.jsonl", line("sess-b", "assistant"))  # same path, offset 0, same text
        self.assertEqual(self.imp("claude", source)["inserted"], 1)
        hits = self.search("одинаковый_текст")
        self.assertEqual({(h["session_id"], h["role"]) for h in hits},
                         {("sess-a", "user"), ("sess-b", "assistant")})
        for hit in hits:
            _, rec, _ = self.run_cli("read", str(hit["id"]))
            self.assertEqual(rec["locator"], "offset:0")

    def test_hermes_row_rewritten_same_text_new_identity(self):
        source = self.make_hermes()
        self.imp("hermes", source)
        conn = sqlite3.connect(source)
        conn.execute("UPDATE messages SET session_id = 'h2' WHERE id = 2")
        conn.execute("UPDATE messages SET role = 'user' WHERE id = 5")
        conn.commit()
        conn.close()
        data = self.imp("hermes", source)
        # The rewritten conversation rows are new identities; the journal rows
        # of untouched rows are recognised as duplicates.
        self.assertEqual((data["inserted"], data["duplicates"]), (2, 2))
        self.assertEqual(self.imp("hermes", source)["inserted"], 0)

    def test_claude_uuid_relocation_deduped(self):
        source = self.write("mv.jsonl", jl(claude_line("user", "перемещённое", "u1")))
        self.imp("claude", source)
        self.write("mv.jsonl", jl(claude_line("user", "новое_сверху", "u0"),
                                  claude_line("user", "перемещённое", "u1")))
        data = self.imp("claude", source)
        self.assertEqual((data["inserted"], data["duplicates"]), (1, 1))
        self.assertEqual(len(self.search("перемещённое")), 1)

    def test_hermes_row_cap_bounds_null_and_blob_rows(self):
        path = os.path.join(self.tmp, "nulls.db")
        conn = sqlite3.connect(path)
        conn.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT,"
                     " role TEXT, content)")
        conn.executemany("INSERT INTO messages(session_id, role, content) VALUES ('h', 'user', ?)",
                         [(None,), (b"\x00blob",)] * 5)
        conn.commit()
        conn.close()
        with mock.patch.object(sa, "HERMES_MAX_ROWS", 3):
            data = self.imp("hermes", path)
            self.assertEqual((data["scanned"], data["inserted"]), (3, 0))
            self.assertTrue(data["scan_truncated"])
            self.assertTrue(data["source_bounded_or_partial"])
            self.assertEqual((data["skipped"], data["malformed"]), (2, 1))
        with mock.patch.object(sa, "HERMES_MAX_ROWS", 10):  # exactly at cap: lookahead finds none
            data = self.imp("hermes", path)
            self.assertEqual(data["scanned"], 10)
            self.assertFalse(data["scan_truncated"])

    def scan_with_cap(self, source, cap):
        positions = []

        class Tracked:
            def __init__(self, handle):
                self.handle = handle

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                positions.append(self.handle.tell())
                self.handle.close()

            def __getattr__(self, name):
                return getattr(self.handle, name)

        with mock.patch.object(sa, "MAX_SCAN", cap), \
                mock.patch.object(sa, "open", create=True,
                                  new=lambda path, mode: Tracked(open(path, mode))):
            data = self.imp("claude", source)
        self.assertEqual(len(positions), 1)
        self.assertLessEqual(positions[0], cap)
        return data

    def test_jsonl_scan_cap_boundary(self):
        lines = [jl(claude_line("user", f"line {i}", f"u{i}")).encode() for i in range(3)]
        size = len(lines[0])
        self.assertTrue(all(len(line) == size for line in lines))
        source = self.write("cap.jsonl", b"".join(lines).decode())
        cases = {2 * size: (2, True), 2 * size - 1: (1, True), 3 * size: (3, False)}
        for cap, (inserted, truncated) in cases.items():
            if os.path.exists(self.db):
                os.remove(self.db)
            data = self.scan_with_cap(source, cap)
            self.assertEqual((data["inserted"], data["scan_truncated"], data["partial_tail"]),
                             (inserted, truncated, 0), cap)
            self.assertEqual(data["source_bounded_or_partial"], truncated, cap)
        tail = self.write("tail.jsonl", lines[0].decode() + lines[1].decode()[:10])
        os.remove(self.db)
        data = self.scan_with_cap(tail, size + 10)  # file ends exactly at the cap
        self.assertEqual((data["inserted"], data["scan_truncated"], data["partial_tail"]),
                         (1, False, 1))

    def test_max_records_bound(self):
        source = self.write("many.jsonl", jl(*[claude_line("user", f"msg {i}", f"u{i}")
                                               for i in range(5)]))
        code, data, _ = self.run_cli("import", "--client", "claude", "--source", source,
                                     "--max-records", "3")
        self.assertEqual((code, data["inserted"], data["record_limit_reached"]), (0, 3, True))
        self.assertEqual(self.imp("claude", source)["inserted"], 2)

    # ------------------------------------------------------------ safety

    def test_source_equals_destination_rejected(self):
        self.imp("claude", self.claude_fixture())
        code, _, _ = self.run_cli("import", "--client", "hermes", "--source", self.db)
        self.assertEqual(code, 2)
        link = os.path.join(self.tmp, "alias.db")
        os.symlink(self.db, link)
        code, _, _ = self.run_cli("import", "--client", "hermes", "--source", link)
        self.assertEqual(code, 2)

    def test_unknown_destination_rejected(self):
        brain = os.path.join(self.tmp, "brain.db")
        conn = sqlite3.connect(brain)
        conn.execute("CREATE TABLE memories(id INTEGER PRIMARY KEY, body TEXT)")
        conn.commit()
        conn.close()
        os.chmod(brain, 0o600)  # owner-only, so the schema check is what rejects it
        before = self.file_sha(brain)
        code, _, text = self.run_cli("import", "--client", "claude",
                                     "--source", self.claude_fixture(), db=brain)
        self.assertEqual(code, 2, text)
        self.assertIn("unknown database schema", text)
        self.assertEqual(self.file_sha(brain), before)
        junk = self.write("junk.db", "not a database at all " * 100)
        os.chmod(junk, 0o600)
        code, _, text = self.run_cli("import", "--client", "claude",
                                     "--source", self.claude_fixture(), db=junk)
        self.assertEqual(code, 2)
        self.assertIn("not a readable SQLite archive", text)

    def test_existing_permissive_archive_rejected_before_write(self):
        os.makedirs(os.path.dirname(self.db), 0o700)
        open(self.db, "wb").close()
        for bad_mode in (0o644, 0o640, 0o604):
            os.chmod(self.db, bad_mode)
            code, _, text = self.run_cli("import", "--client", "claude",
                                         "--source", self.claude_fixture())
            self.assertEqual(code, 2, text)
            self.assertIn("owner-only", text)
            self.assertEqual(os.path.getsize(self.db), 0)
            self.assertEqual(stat.S_IMODE(os.stat(self.db).st_mode), bad_mode)
        os.chmod(self.db, 0o600)  # empty owner-only file is initialized
        self.assertEqual(self.imp("claude", self.claude_fixture())["inserted"], 5)
        os.chmod(self.db, 0o644)
        before = self.file_sha(self.db)
        source = self.write("more.jsonl", jl(claude_line("user", "новое", "u9")))
        code, _, text = self.run_cli("import", "--client", "claude", "--source", source)
        self.assertEqual(code, 2, text)
        self.assertEqual(self.file_sha(self.db), before)
        self.assertFalse(os.path.exists(self.db + "-journal"))

    def test_symlinked_destination_rejected(self):
        self.imp("claude", self.claude_fixture())
        link = os.path.join(self.tmp, "link.db")
        os.symlink(self.db, link)
        code, _, _ = self.run_cli("import", "--client", "claude",
                                  "--source", self.claude_fixture(), db=link)
        self.assertEqual(code, 2)
        self.assertEqual(self.run_cli("status", db=link)[0], 2)

    def test_missing_destination_read_commands_create_nothing(self):
        before = sorted(os.listdir(self.tmp))
        for argv in (("search", "x"), ("read", "1"), ("status",)):
            code, _, text = self.run_cli(*argv)
            self.assertEqual(code, 2)
            self.assertIn("not found", text)
        self.assertFalse(os.path.lexists(os.path.dirname(self.db)))
        self.assertEqual(sorted(os.listdir(self.tmp)), before)

    def test_private_modes(self):
        existing = os.path.join(self.tmp, "shared")
        os.mkdir(existing, 0o755)
        os.chmod(existing, 0o755)
        db = os.path.join(existing, "new", "deeper", "a.db")
        code, _, text = self.run_cli("import", "--client", "claude",
                                     "--source", self.claude_fixture(), db=db)
        self.assertEqual(code, 0, text)
        mode = lambda p: stat.S_IMODE(os.stat(p).st_mode)
        self.assertEqual(mode(existing), 0o755)
        self.assertEqual(mode(os.path.join(existing, "new")), 0o700)
        self.assertEqual(mode(os.path.dirname(db)), 0o700)
        self.assertEqual(mode(db), 0o600)
        self.assertFalse(os.path.exists(db + "-wal"))

    def test_shared_writable_parent_rejected(self):
        shared = os.path.join(self.tmp, "shared")
        os.mkdir(shared)
        os.chmod(shared, 0o777)
        code, _, text = self.run_cli("import", "--client", "claude", "--source",
                                     self.claude_fixture(), db=os.path.join(shared, "a.db"))
        self.assertEqual(code, 2)
        self.assertIn("not group/other", text)
        self.assertEqual(os.listdir(shared), [])

    def test_journal_side_file_symlink_rejected(self):
        source = self.claude_fixture()
        self.imp("claude", source)
        target = self.write("victim.txt", "untouched")
        os.symlink(target, self.db + "-journal")
        code, _, _ = self.run_cli("import", "--client", "claude", "--source", source)
        self.assertEqual(code, 2)
        with open(target, encoding="utf-8") as handle:
            self.assertEqual(handle.read(), "untouched")

    def test_hermes_byte_budget_counts_skipped_rows(self):
        path = os.path.join(self.tmp, "big.db")
        conn = sqlite3.connect(path)
        conn.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT,"
                     " role TEXT, content)")
        conn.executemany("INSERT INTO messages(session_id, role, content) VALUES ('h', ?, ?)",
                         [("user", "x" * 400), ("user", "y" * 400), ("user", "late_word")])
        conn.commit()
        conn.close()
        # Oversize rows are skipped unread, yet their bytes still count against the budget.
        with mock.patch.object(sa, "MAX_SCAN", 600), mock.patch.object(sa, "MAX_LINE", 100):
            data = self.imp("hermes", path)
        self.assertTrue(data["scan_truncated"])
        self.assertEqual((data["oversize"], data["inserted"]), (1, 0))

    def test_invalid_byte_line_and_string_codex_content(self):
        good = jl({"type": "response_item", "payload": {
            "type": "message", "role": "user", "content": "строковый_контент"}})
        path = os.path.join(self.tmp, "codex.jsonl")
        with open(path, "wb") as handle:
            handle.write(b"\xff\xfe secret_bytes\n" + good.encode("utf-8"))
        code, data, text = self.run_cli("import", "--client", "codex", "--source", path)
        self.assertEqual(code, 0, text)
        self.assertEqual((data["malformed"], data["inserted"]), (1, 1))
        self.assertNotIn("secret_bytes", text)
        self.assertEqual(len(self.search("строковый_контент")), 1)

    def test_max_records_out_of_range_rejected(self):
        source = self.claude_fixture()
        for bad in ("0", "-1", str(sa.DEFAULT_MAX_RECORDS + 1)):
            code, _, _ = self.run_cli("import", "--client", "claude", "--source", source,
                                      "--max-records", bad)
            self.assertEqual(code, 2, bad)

    def test_inline_reasoning_strip_is_case_insensitive_and_leading_only(self):
        strip = sa._strip_inline_reasoning
        self.assertEqual(strip("<THINK>a</Think>b"), "b")
        self.assertEqual(strip(" <think>a</think>\n<think>b</think>c"), "c")
        self.assertIsNone(strip("<think>unclosed"))
        self.assertEqual(strip("İ <think>quoted</think>"), "İ <think>quoted</think>")

    def test_read_trust_marker_and_truncation(self):
        big = "длинный_текст " * 4000
        source = self.write("r.jsonl", jl(claude_line("user", "короткое сообщение", "u1"),
                                          claude_line("assistant", big, "a1")))
        self.imp("claude", source)
        short_id = self.search("короткое")[0]["id"]
        code, rec, _ = self.run_cli("read", str(short_id))
        self.assertEqual(code, 0)
        self.assertEqual(rec["trust"], "untrusted_conversation")
        self.assertFalse(rec["truncated"])
        self.assertEqual(rec["content"], "короткое сообщение")
        self.assertEqual(rec["content_sha256"],
                         hashlib.sha256("короткое сообщение".encode()).hexdigest())
        self.assertEqual((rec["locator"], rec["source_offset"]), ("uuid:u1", 0))
        self.assertNotIn(self.tmp, json.dumps(rec))
        long_id = self.search("длинный_текст")[0]["id"]
        _, rec, _ = self.run_cli("read", str(long_id))
        self.assertTrue(rec["truncated"])
        self.assertLessEqual(len(rec["content"].encode()), sa.READ_MAX)
        self.assertEqual(self.run_cli("read", "999")[0], 2)

    def test_fts_injection_literal(self):
        source = self.write("f.jsonl", jl(claude_line("user", "alpha only", "u1"),
                                          claude_line("user", "beta only", "u2"),
                                          claude_line("user", 'quote " and NEAR text', "u3")))
        self.imp("claude", source)
        self.assertEqual(self.search("alpha OR beta"), [])
        self.assertEqual(self.search("alpha NOT beta"), [])
        self.assertEqual(len(self.search("NEAR")), 1)
        for hostile in ('"', 'a" OR "b', "content:alpha", "alpha*", "NEAR(alpha beta)",
                        "^alpha", "(", "'; DROP TABLE entries; --"):
            code, _, text = self.run_cli("search", hostile)
            self.assertEqual(code, 0, f"{hostile!r}: {text}")
        self.assertEqual(self.search("content:alpha"), [])
        _, status, _ = self.run_cli("status")
        self.assertEqual(status["entries"], 3)

    def test_search_limit_and_status_counts_only(self):
        source = self.write("n.jsonl", jl(*[claude_line("user", "needle " + "слово " * 200, f"u{i}")
                                            for i in range(30)]))
        self.imp("claude", source)
        results = self.search("needle", limit=50)
        self.assertEqual(len(results), sa.SEARCH_MAX)
        self.assertTrue(all(len(r["excerpt"]) <= sa.EXCERPT_MAX for r in results))
        code, status, text = self.run_cli("status")
        self.assertEqual(code, 0)
        self.assertEqual(status["entries"], 30)
        self.assertNotIn("needle", text)
        self.assertNotIn(self.tmp, text)


if __name__ == "__main__":
    unittest.main()
