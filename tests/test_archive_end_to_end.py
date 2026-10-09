"""End to end: the real `brain` binary collects fixture conversations of all three
clients through its bundled collector, then reads the same evidence via CLI and MCP.

    BRAIN_BIN=target/release/brain python3 -I tests/test_archive_end_to_end.py
"""
import json
import os
import shutil
import sqlite3
import subprocess
import tempfile
import unittest

BRAIN = os.environ.get("BRAIN_BIN")


def jl(*objs):
    return "".join(json.dumps(o, ensure_ascii=False) + "\n" for o in objs)


@unittest.skipUnless(BRAIN, "set BRAIN_BIN to the brain binary under test")
class ArchiveEndToEnd(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.mkdtemp(prefix="brain-e2e-")
        os.chmod(self.home, 0o700)
        self.env = {"HOME": self.home, "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
                    "BRAIN_OFFLINE": "true"}
        claude = os.path.join(self.home, ".claude/projects/-work-e2e/sess.jsonl")
        os.makedirs(os.path.dirname(claude))
        with open(claude, "w", encoding="utf-8") as handle:
            handle.write(jl({"type": "user", "sessionId": "c-1", "uuid": "u1", "cwd": "/work/e2e",
                             "timestamp": "2026-10-08T10:00:00Z",
                             "message": {"role": "user", "content": "фикстура_claude_e2e вопрос"}}))
        self.codex = os.path.join(self.home, ".codex/sessions/2026/10/08/rollout-e2e.jsonl")
        os.makedirs(os.path.dirname(self.codex))
        with open(self.codex, "w", encoding="utf-8") as handle:
            handle.write(jl({"type": "session_meta", "payload": {"id": "x-1", "cwd": "/work/e2e"}},
                            {"type": "response_item", "timestamp": "2026-10-08T10:01:00Z",
                             "payload": {"type": "message", "role": "assistant", "content": [
                                 {"type": "output_text", "text": "фикстура_codex_e2e ответ"}]}}))
        hermes = os.path.join(self.home, ".hermes/state.db")
        os.makedirs(os.path.dirname(hermes))
        conn = sqlite3.connect(hermes)
        conn.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT,"
                     " content TEXT, timestamp REAL)")
        conn.execute("INSERT INTO messages(session_id, role, content, timestamp)"
                     " VALUES ('h-1', 'user', 'фикстура_hermes_e2e привет', 1791453600)")
        conn.commit()
        conn.close()

    def tearDown(self):
        shutil.rmtree(self.home, ignore_errors=True)

    def brain(self, *args, ok=True):
        proc = subprocess.run([BRAIN, *args], env=self.env, capture_output=True, text=True,
                              timeout=180)
        if ok:
            self.assertEqual(proc.returncode, 0, proc.stderr)
            return json.loads(proc.stdout)
        return proc

    def mcp(self, *calls):
        lines = [json.dumps({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}})]
        for index, (name, arguments) in enumerate(calls, 1):
            lines.append(json.dumps({"jsonrpc": "2.0", "id": index, "method": "tools/call",
                                     "params": {"name": name, "arguments": arguments}}))
        proc = subprocess.run([BRAIN, "serve"], env=self.env, input="\n".join(lines) + "\n",
                              capture_output=True, text=True, timeout=60)
        answers = [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]
        return [a["result"]["structuredContent"] for a in answers[1:]]

    def test_three_clients_collected_then_read_through_cli_and_mcp(self):
        missing = self.brain("archive", "status")
        self.assertEqual(missing["status"], "not_configured")
        (mcp_missing,) = self.mcp(("archive_search", {"query": "x"}))
        self.assertEqual(mcp_missing["error_kind"], "not_configured")
        paused = self.brain("archive", "collect")
        self.assertEqual(paused["status"], "not_configured")
        self.brain("archive", "enable", "--since-days", "3650")
        first = self.brain("archive", "collect")
        self.assertEqual(first["status"], "ok")
        self.assertEqual(first["stored_entries"], {"claude": 1, "codex": 1, "hermes": 1})
        for client in ("claude", "codex", "hermes"):
            word = f"фикстура_{client}_e2e"
            cli = self.brain("archive", "search", word)
            self.assertEqual(len(cli["results"]), 1, client)
            hit = cli["results"][0]
            self.assertEqual((hit["client"], cli["trust"]), (client, "untrusted_conversation"))
            read = self.brain("archive", "read", str(hit["id"]))
            search_mcp, read_mcp = self.mcp(("archive_search", {"query": word}),
                                            ("archive_read", {"id": hit["id"]}))
            self.assertEqual(search_mcp["results"][0]["id"], hit["id"])
            for field in ("content", "content_sha256", "evidence_sha256", "session_id",
                          "observed_at", "client", "trust"):
                self.assertEqual(read_mcp[field], read[field], field)
            self.assertIn(word, read["content"])
        project = self.brain("archive", "search", "фикстура_codex_e2e", "--project-root", "/work/e2e")
        self.assertEqual(len(project["results"]), 1)
        second = self.brain("archive", "collect")
        self.assertEqual(sum(c["inserted"] for c in second["clients"].values()), 0)
        with open(self.codex, "a", encoding="utf-8") as handle:
            handle.write(jl({"type": "response_item", "timestamp": "2026-10-08T10:02:00Z",
                             "payload": {"type": "message", "role": "user", "content": [
                                 {"type": "input_text", "text": "дописано_e2e"}]}}))
        third = self.brain("archive", "collect")
        self.assertEqual(third["clients"]["codex"]["inserted"], 1)
        self.brain("archive", "collect")
        self.assertEqual(len(self.brain("archive", "search", "дописано_e2e")["results"]), 1)
        status = self.brain("archive", "status")
        self.assertEqual(status["stored"]["entries"], 4)
        self.assertFalse(status["complete_capture_claimed"])
        self.assertEqual(status["observed"]["runs_recorded"], 4)
        helper = os.path.join(self.home, ".brain/archive/collector.py")
        self.assertEqual(os.stat(helper).st_mode & 0o777, 0o600)
        self.assertEqual(os.stat(os.path.join(self.home, ".brain/archive/sessions.sqlite")).st_mode
                         & 0o777, 0o600)
        self.brain("archive", "disable")
        self.assertEqual(self.brain("archive", "collect")["status"], "disabled")
        (tools,) = [json.loads(line)["result"]["tools"] for line in subprocess.run(
            [BRAIN, "serve"], env=self.env, capture_output=True, text=True, timeout=60,
            input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}) + "\n"
        ).stdout.splitlines()]
        # The four map tools extend the existing 23-tool catalog. Check
        # their identity as well as the bound, so duplicates cannot pass.
        self.assertEqual(len(tools), 27)
        names = {tool["name"] for tool in tools}
        self.assertEqual(len(names), len(tools))
        self.assertLessEqual({
            "archive_status", "archive_search", "archive_read",
            "map_status", "map_timeline", "map_packet", "map_ingest_coordinator",
        }, names)


if __name__ == "__main__":
    unittest.main()
