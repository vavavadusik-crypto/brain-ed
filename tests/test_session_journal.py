"""Journal capture: tool calls, tool output, errors and visible reasoning, secret-masked.

Reuses the collector fixture (a synthetic HOME with Claude, Codex and Hermes sessions);
only the tests defined here run from this file.
"""
import importlib.util
import json
import os
import sqlite3
import sys
import unittest
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "test_session_collector", os.path.join(HERE, "test_session_collector.py"))
tc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tc)
sa, jl, claude, codex, codex_meta = tc.sa, tc.jl, tc.claude, tc.codex, tc.codex_meta

FAKE_KEY = "sk-ant-" + "A1b2C3d4E5f6G7h8I9j0K1l2"  # assembled so no scanner sees a literal key


class JournalTest(tc.CollectorTest):
    def rows(self, role):
        conn = sqlite3.connect(self.db)
        try:
            return [r[0] for r in conn.execute(
                "SELECT content FROM entries WHERE role = ? ORDER BY id", (role,))]
        finally:
            conn.close()

    def claude_tools(self, rel=".claude/projects/-work-proj/tools.jsonl"):
        return self.put(rel, jl(
            claude("assistant", [
                {"type": "text", "text": "запускаю тесты"},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash",
                 "input": {"command": "cargo test journal_probe"}},
            ], "t-a1"),
            claude("user", [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": True,
                             "content": [{"type": "text", "text": "error[E0425]: journal_failure"}]}],
                   "t-u1"),
            claude("user", [{"type": "tool_result", "tool_use_id": "toolu_2",
                             "content": "x" * 5000 + " long_output_tail"}], "t-u2"),
        ))

    def test_claude_tool_calls_errors_and_results_are_archived(self):
        self.claude_tools()
        self.collect()
        self.assertEqual(self.rows("tool_call"), ['Bash {"command": "cargo test journal_probe"}'])
        self.assertEqual(self.rows("tool_error"), ["error[E0425]: journal_failure"])
        results = self.rows("tool_result")
        # The shared Hermes fixture contributes its own tool_result row now.
        self.assertEqual(results[-1], "shell: tool_secret")
        [long] = results[:-1]
        self.assertLessEqual(len(long), sa.JOURNAL_MAX + 80)
        self.assertIn("more characters not archived", long)
        self.assertNotIn("long_output_tail", long)
        self.assertEqual([h["role"] for h in self.search("journal_failure")], ["tool_error"])
        # The message text keeps its own row and identity beside the tool call.
        self.assertEqual(len(self.search("запускаю")), 1)

    def test_codex_calls_outputs_and_reasoning_summary(self):
        self.put(".codex/sessions/2026/10/08/rollout-tools.jsonl", jl(
            codex_meta("sess-tools"),
            {"type": "response_item", "timestamp": "2026-10-08T09:01:00Z", "payload": {
                "type": "function_call", "name": "shell", "call_id": "c1",
                "arguments": "{\"command\":[\"rg\",\"codex_probe\"]}"}},
            {"type": "response_item", "timestamp": "2026-10-08T09:01:01Z", "payload": {
                "type": "function_call_output", "call_id": "c1", "output": "codex_tool_output"}},
            {"type": "response_item", "timestamp": "2026-10-08T09:01:02Z", "payload": {
                "type": "reasoning", "summary": [{"type": "summary_text", "text": "codex_summary_visible"}],
                "encrypted_content": "codex_hidden_cipher", "content": [{"text": "codex_raw_hidden"}]}},
        ))
        self.collect()
        self.assertEqual(self.rows("tool_call"), ['shell {"command":["rg","codex_probe"]}'])
        # The shared Hermes fixture's tool row joins as another tool_result.
        self.assertEqual(self.rows("tool_result"),
                         ["codex_tool_output", "shell: tool_secret"])
        self.assertEqual(self.rows("reasoning")[-1], "codex_summary_visible")
        for hidden in ("codex_hidden_cipher", "codex_raw_hidden"):
            self.assertEqual(self.search(hidden), [], hidden)
        hit = self.search("codex_tool_output")[0]
        self.assertEqual((hit["session_id"], hit["role"]), ("sess-tools", "tool_result"))

    def test_secrets_are_masked_before_storage_in_every_role(self):
        self.put(".claude/projects/-work-proj/secret.jsonl", jl(
            claude("user", f"мой ключ {FAKE_KEY} не сохраняй", "k-u1"),
            claude("assistant", [{"type": "tool_use", "id": "toolu_9", "name": "Bash",
                                  "input": {"command": f"export API_KEY={FAKE_KEY}"}}], "k-a1"),
        ))
        self.collect()
        conn = sqlite3.connect(self.db)
        try:
            stored = "\n".join(r[0] for r in conn.execute("SELECT content FROM entries"))
            fts = "\n".join(r[0] for r in conn.execute("SELECT content FROM entries_fts"))
        finally:
            conn.close()
        self.assertNotIn(FAKE_KEY, stored)
        self.assertNotIn(FAKE_KEY, fts)
        self.assertIn("[REDACTED]", stored)
        self.assertEqual(len(self.search("сохраняй")), 1)

    def test_shared_secret_vectors_match_mask_secrets_byte_for_byte(self):
        """tests/secret_cases.json is the one list both maskers answer to.

        Parts are glued at test time so scanners never see a full literal;
        idempotency is part of the contract: masking an already-masked text
        changes nothing.
        """
        vectors = json.loads(
            (Path(__file__).resolve().parent / "secret_cases.json").read_text(encoding="utf-8")
        )
        self.assertTrue(vectors)
        for case in vectors:
            with self.subTest(case["name"]):
                text = "".join(case["parts"])
                expected = "".join(case["expected_parts"])
                self.assertEqual(sa._mask_secrets(text), expected)
                self.assertEqual(sa._mask_secrets(expected), expected)

    def test_mask_patterns_cover_common_credentials_and_leave_prose(self):
        parts = {
            "github": "ghp_" + "a" * 36,
            "aws": "AKIA" + "ABCDEFGHIJKLMNOP",
            "slack": "xoxb-" + "1234567890-abcdef",
            "jwt": "eyJ" + "a" * 12 + "." + "b" * 12 + "." + "c" * 12,
            "pem": "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----",
        }
        for name, secret in parts.items():
            self.assertNotIn(secret, sa._mask_secrets(f"value: {secret} end"), name)
        self.assertEqual(sa._mask_secrets("password: hunter2longvalue1"), "password: [REDACTED]")
        prose = "токен доступа истёк, перезапусти login; tokenizer unicode61"
        self.assertEqual(sa._mask_secrets(prose), prose)

    def test_env_json_and_url_credentials_are_masked(self):
        value = "Fake" + "Value" + "9x!@#$"
        shapes = [
            f"DB_PASSWORD={value}",
            f"export GITHUB_TOKEN={value}",
            f"AWS_SECRET_ACCESS_KEY={value}",
            f'{{"password": "{value}"}}',
            f'{{"api_key":"{value}", "n": 1}}',
            f"X-Api-Key: {value}",
            f"postgres://app:{value}@db.local/main",
        ]
        for shape in shapes:
            masked = sa._mask_secrets(shape)
            self.assertNotIn(value, masked, shape)
            self.assertIn("[REDACTED]", masked, shape)
            self.assertEqual(sa._mask_secrets(masked), masked, "masking twice changes nothing")
        basic = "dXNlcjpGYWtl" + "VmFsdWU5eA=="
        self.assertEqual(sa._mask_secrets(f"Authorization: Basic {basic}"),
                         "Authorization: Basic [REDACTED]")
        self.assertEqual(sa._mask_secrets(f"postgres://app:{value}@db.local/main"),
                         "postgres://app:[REDACTED]@db.local/main")
        self.assertEqual(sa._mask_secrets(f'{{"password": "{value}"}}'), '{"password": "[REDACTED]"}')

    def test_masking_leaves_counts_and_tokenizer_settings_readable(self):
        for text in ("max_tokens=4096", '{"token_count": 1200, "input_tokens": 99999999}',
                     "tokenize='unicode61 remove_diacritics 2'", "token_budget: 2400",
                     "password: неверный пароль", "https://example.com:8443/path",
                     "fixtureuser ALL=(ALL) NOPASSWD: /usr/bin/systemctl", "credentials: 'include'"):
            self.assertEqual(sa._mask_secrets(text), text)

    def test_mask_upgrade_masks_new_shapes_in_stored_rows_without_rereading(self):
        self.collect()
        conn = sqlite3.connect(self.db)
        entry_id, content = conn.execute(
            "SELECT id, content FROM entries WHERE role = 'user' LIMIT 1").fetchone()
        leaked = content + " DB_PASSWORD=Fake" + "Value9xQ"
        conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                     (entry_id, content))
        conn.execute("UPDATE entries SET content = ? WHERE id = ?", (leaked, entry_id))
        conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (entry_id, leaked))
        conn.execute("DELETE FROM archive_meta WHERE key = 'mask_rev'")
        conn.commit()
        conn.close()
        data = self.collect()
        self.assertEqual(data["clients"]["claude"]["resets"], 0, "no source is read again")
        conn = sqlite3.connect(self.db)
        try:
            text = conn.execute("SELECT content FROM entries WHERE id = ?", (entry_id,)).fetchone()[0]
            fts = conn.execute("SELECT count(*) FROM entries_fts WHERE entries_fts MATCH 'FakeValue9xQ'"
                               ).fetchone()[0]
            rev = conn.execute("SELECT value FROM archive_meta WHERE key = 'mask_rev'").fetchone()[0]
        finally:
            conn.close()
        self.assertIn("DB_PASSWORD=[REDACTED]", text)
        self.assertEqual(fts, 0)
        self.assertEqual(rev, sa.MASK_REV)

    def test_mask_upgrade_merges_versions_that_differ_only_in_a_secret(self):
        self.collect()
        conn = sqlite3.connect(self.db)
        cols = [r[1] for r in conn.execute("PRAGMA table_info(entries)") if r[1] != "id"]
        entry_id = conn.execute("SELECT id FROM entries WHERE role = 'user' LIMIT 1").fetchone()[0]
        original = conn.execute(f"SELECT {', '.join(cols)} FROM entries WHERE id = ?",
                                (entry_id,)).fetchone()
        row = dict(zip(cols, original))
        base = row["content"]
        versions = [base + " DB_PASSWORD=Fake" + "Value9xQ", base + " DB_PASSWORD=Other" + "Value7zR"]
        conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                     (entry_id, base))
        conn.execute("UPDATE entries SET content = ?, evidence_sha256 = 'pre-mask-a' WHERE id = ?",
                     (versions[0], entry_id))
        conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (entry_id, versions[0]))
        row.update(content=versions[1], evidence_sha256="pre-mask-b")
        twin = conn.execute(f"INSERT INTO entries({', '.join(cols)}) VALUES ({', '.join('?' * len(cols))})",
                            [row[c] for c in cols]).lastrowid
        conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (twin, versions[1]))
        conn.execute("DELETE FROM archive_meta WHERE key = 'mask_rev'")
        conn.commit()
        conn.close()
        self.collect()
        conn = sqlite3.connect(self.db)
        try:
            stored = [r[0] for r in conn.execute(
                "SELECT content FROM entries WHERE id IN (?, ?)", (entry_id, twin))]
            rev = conn.execute("SELECT value FROM archive_meta WHERE key = 'mask_rev'").fetchone()
            fts = conn.execute("SELECT count(*) FROM entries_fts WHERE entries_fts MATCH 'DB_PASSWORD'"
                               ).fetchone()[0]
        finally:
            conn.close()
        self.assertEqual(stored, [base + " DB_PASSWORD=[REDACTED]"])
        self.assertEqual(rev, (sa.MASK_REV,))
        self.assertEqual(fts, 1)

    def test_r5_secret_shapes_are_masked(self):
        secret, short = "Synth" + "etic9Qz", "aB" + "9x"
        for text, value in [
            ("api_key: >-\n  " + secret, secret),
            ("machine example.invalid login alice password " + secret, secret),
            ("--password " + secret, secret),
            ("-p'" + secret + "'", secret),
            ('export DB_PASSWORD="abcdef ' + secret + '"', secret),
            ("https://example.invalid/?access_token=" + secret, secret),
            ("X-Auth-Token: " + secret, secret),
            ("PGPASSWORD=" + secret, secret),
            ("aws_secret_access_key = " + secret, secret),
            ("password=123456-" + secret, secret),
            ("password=" + short, short),
        ]:
            masked = sa._mask_secrets(text)
            self.assertNotIn(value, masked, text.split()[0])
            self.assertEqual(sa._mask_secrets(masked), masked)

    def test_long_and_unclosed_values_are_masked_whole(self):
        long = ("Ab9" * 300)[:600]
        for text in (f'password: "{long[:300]}"', f"DB_PASSWORD={long}", f"api_key='{long[:40]} no close",
                     f"Authorization: Bearer {long}", f"https://u:{long[:300]}@host/x",
                     "password=$abc9De" + "f.part", "max_token=abc123" + "defXY", f"--password {long}"):
            masked = sa._mask_secrets(text)
            self.assertNotIn(long[:12], masked.replace("[REDACTED]", ""), text[:20])
            self.assertNotIn("abc9Def", masked, text[:20])
            self.assertNotIn("abc123defXY", masked, text[:20])
            self.assertEqual(sa._mask_secrets(masked), masked, text[:20])
        self.assertEqual(sa._mask_secrets("api_key='Syn" + "thetic9Qz"), "api_key='[REDACTED]")

    def test_r5_code_and_type_names_stay_readable(self):
        for text in ("token = parser.next_token()", 'password = os.environ["DB_PASSWORD"]',
                     '{"token": "string"}', "password: ${DB_PASSWORD}", "the token expired yesterday",
                     "keep the secret sauce", "--password-file /run/secrets/db",
                     "--api-key OPENAI_API_KEY", "--token <your-token>", "token usage: 1200"):
            self.assertEqual(sa._mask_secrets(text), text)

    def test_r5_long_lines_mask_in_linear_time(self):
        import time
        for text in ("a-" * 131072, "password " * 30000, "x:" * 131072, "_a" * 131072):
            started = time.monotonic()
            sa._mask_secrets(text)
            self.assertLess(time.monotonic() - started, 2.0, text[:12])

    def test_r5_merging_twins_keeps_an_owner_release(self):
        self.collect()
        conn = sqlite3.connect(self.db)
        cols = [r[1] for r in conn.execute("PRAGMA table_info(entries)") if r[1] != "id"]
        keep = conn.execute("SELECT id FROM entries WHERE role = 'user' LIMIT 1").fetchone()[0]
        row = dict(zip(cols, conn.execute(f"SELECT {', '.join(cols)} FROM entries WHERE id = ?",
                                          (keep,)).fetchone()))
        base = row["content"]
        conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                     (keep, base))
        conn.execute("UPDATE entries SET content = ? WHERE id = ?", (base + " DB_PASSWORD=Fake" + "One9xQ", keep))
        row.update(content=base + " DB_PASSWORD=Fake" + "Two7zR", evidence_sha256="pre-mask-b")
        drop = conn.execute(f"INSERT INTO entries({', '.join(cols)}) VALUES ({', '.join('?' * len(cols))})",
                            [row[c] for c in cols]).lastrowid
        for entry_id in (keep, drop):
            conn.execute("INSERT INTO quarantine VALUES (?, 'synthetic', 'rule', '{}', 'now', ?)",
                         (entry_id, "owner-release" if entry_id == drop else None))
        conn.execute("DELETE FROM archive_meta WHERE key = 'mask_rev'")
        conn.commit()
        conn.close()
        self.collect()
        conn = sqlite3.connect(self.db)
        try:
            left = conn.execute("SELECT entry_id, released_at FROM quarantine WHERE entry_id IN (?, ?)",
                                (keep, drop)).fetchall()
        finally:
            conn.close()
        self.assertEqual(left, [(keep, "owner-release")])

    def test_r5_reimport_masks_a_raw_row_in_place(self):
        source = self.put(".claude/projects/-work-proj/legacy.jsonl",
                          jl(claude("user", "legacy probe DB_PASSWORD=Fake" + "Legacy9xQ", "legacy-1")))
        self.collect()
        conn = sqlite3.connect(self.db)
        entry_id, masked, session, role, ts = conn.execute(
            "SELECT id, content, session_id, role, ts FROM entries WHERE content LIKE 'legacy probe%'").fetchone()
        raw = "legacy probe DB_PASSWORD=Fake" + "Legacy9xQ"
        conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                     (entry_id, masked))
        conn.execute("UPDATE entries SET content = ?, evidence_sha256 = ? WHERE id = ?",
                     (raw, sa._evidence_sha(session, role, ts, raw), entry_id))
        conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (entry_id, raw))
        conn.execute("DELETE FROM cursors")  # read the source again from the start
        conn.commit()
        conn.close()
        self.collect()
        conn = sqlite3.connect(self.db)
        try:
            rows = [r[0] for r in conn.execute("SELECT content FROM entries WHERE content LIKE 'legacy probe%'")]
        finally:
            conn.close()
        self.assertEqual(rows, [masked])

    def test_capture_upgrade_rereads_history_without_duplicating_conversation(self):
        self.claude_tools()
        self.collect()
        conversation = self.entries()
        conn = sqlite3.connect(self.db)
        conn.execute("DELETE FROM entries_fts WHERE rowid IN"
                     " (SELECT id FROM entries WHERE role NOT IN ('user', 'assistant'))")
        conn.execute("DELETE FROM entries WHERE role NOT IN ('user', 'assistant')")
        conn.execute("DELETE FROM archive_meta WHERE key = 'capture_rev'")  # an older archive
        conn.commit()
        conn.close()
        data = self.collect()
        self.assertEqual(self.entries(), conversation)  # same rows, nothing doubled
        self.assertEqual(len(self.rows("tool_call")), 1)
        self.assertGreaterEqual(data["clients"]["claude"]["resets"], 1)
        again = self.collect()
        self.assertEqual(again["clients"]["claude"]["inserted"], 0)

    def test_upgrade_masks_secrets_already_stored_and_keeps_rows(self):
        self.collect()
        conn = sqlite3.connect(self.db)
        entry_id, content = conn.execute(
            "SELECT id, content FROM entries WHERE role = 'user' LIMIT 1").fetchone()
        leaked = content + f" leaked {FAKE_KEY}"
        conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                     (entry_id, content))
        conn.execute("UPDATE entries SET content = ? WHERE id = ?", (leaked, entry_id))
        conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (entry_id, leaked))
        conn.execute("DELETE FROM archive_meta WHERE key = 'capture_rev'")
        conn.commit()
        before = conn.execute("SELECT count(*) FROM entries").fetchone()[0]
        conn.close()
        self.collect()
        conn = sqlite3.connect(self.db)
        try:
            text = conn.execute("SELECT content FROM entries WHERE id = ?", (entry_id,)).fetchone()[0]
            after = conn.execute("SELECT count(*) FROM entries WHERE role IN ('user','assistant')"
                                 ).fetchone()[0]
        finally:
            conn.close()
        self.assertNotIn(FAKE_KEY, text)
        self.assertIn("[REDACTED]", text)
        self.assertEqual(self.search("leaked")[0]["id"], entry_id)
        self.assertLessEqual(after, before)  # masked in place, not stored again beside the original

    def test_quarantine_sweep_never_hides_journal_rows(self):
        self.put(".codex/sessions/2026/10/08/rollout-q.jsonl", jl(
            codex_meta("sess-q"),
            {"type": "response_item", "timestamp": "2026-10-08T09:02:00Z", "payload": {
                "type": "function_call_output", "call_id": "q1", "output": "sweep_probe_output"}},
        ))
        self.collect()
        conn = sqlite3.connect(self.db)
        conn.execute("DELETE FROM archive_meta WHERE key = 'quarantine_rule'")  # force a sweep
        conn.commit()
        conn.close()
        self.collect()
        self.assertEqual([h["role"] for h in self.search("sweep_probe_output")], ["tool_result"])


class ImportedSessionTest(tc.CollectorTest):
    def test_named_external_tool_markers_are_quarantined_and_prose_mentions_kept(self):
        self.put(".codex/sessions/2026/10/08/rollout-import.jsonl", jl(
            codex_meta("sess-import"),
            codex("assistant", "[external_agent_tool_call: Bash]\nimported_call_probe", 1),
            codex("assistant", "[external_agent_tool_result: error]\nimported_error_probe", 2),
            codex("assistant", "the [external_agent_tool_call: Bash] marker means prose_probe", 3),
        ))
        self.collect()
        self.assertEqual(self.search("imported_call_probe"), [])
        self.assertEqual(self.search("imported_error_probe"), [])
        self.assertEqual([h["role"] for h in self.search("prose_probe")], ["assistant"])


class HermesMaskingTest(tc.CollectorTest):
    def test_hermes_message_text_is_masked_and_a_reread_adds_no_unmasked_copy(self):
        path = os.path.join(self.home, ".hermes", "state.db")
        conn = sqlite3.connect(path)
        conn.execute("INSERT INTO messages(session_id, role, content, timestamp)"
                     " VALUES ('h9', 'user', ?, 1783629578.4)", (f"держи {FAKE_KEY} и продолжай",))
        conn.commit()
        conn.close()
        self.collect()
        db = sqlite3.connect(self.db)
        try:
            rows = [r[0] for r in db.execute("SELECT content FROM entries WHERE content LIKE '%продолжай%'")]
        finally:
            db.close()
        self.assertEqual(len(rows), 1)
        self.assertNotIn(FAKE_KEY, rows[0])
        db = sqlite3.connect(self.db)
        db.execute("DELETE FROM archive_meta WHERE key = 'capture_rev'")  # force a full re-read
        db.commit()
        db.close()
        self.collect()
        db = sqlite3.connect(self.db)
        try:
            again = [r[0] for r in db.execute("SELECT content FROM entries WHERE content LIKE '%продолжай%'")]
        finally:
            db.close()
        self.assertEqual(again, rows)


def load_tests(loader, tests, pattern):
    suite = unittest.TestSuite()
    for case in (JournalTest, ImportedSessionTest, HermesMaskingTest):
        suite.addTests(case(n) for n in case.__dict__ if n.startswith("test_"))
    return suite


if __name__ == "__main__":
    unittest.main()
