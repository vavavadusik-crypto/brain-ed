"""E7.2 process regression: loopback TLS, two temporary HOMEs, synthetic sources.
No live HOME, archive, SSH, model, external network or daemon is used.
Run with BRAIN_PROBE_BIN pointing at the newly built debug brain.
"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import unittest

BRAIN = os.environ.get("BRAIN_PROBE_BIN")


@unittest.skipUnless(BRAIN, "set BRAIN_PROBE_BIN to the debug brain binary")
class TeamFactsProcess(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="brain-e72-")
        self.base = Path(self.tmp.name)
        self.source = self.base / "machine"
        self.coordinator = self.base / "coordinator"
        self.source.mkdir()
        self.coordinator.mkdir()
        self.state = self.source / ".brain/net"
        self.coordstate = self.coordinator / ".brain/net"
        self.repo = self.source / "project"
        self.repo.mkdir()
        self.db = self.source / ".brain/brain.sqlite"
        self.server = None
        self.transcript = []
        self.machine = "7"
        self.raw = "RAW_SESSION_TRAP_8b7f"
        self.tool = "TOOL_STDOUT_TRAP_c19d"
        self.secret = "sk-" + "syntheticKEY01234567890123456789"
        self.absolute = str(self.source / "private/raw.txt")

    def tearDown(self):
        if self.server:
            self.server.stdin.close()
            self.server.wait(timeout=15)
            self.server.stdout.close()
            self.server.stderr.close()
        self.tmp.cleanup()

    def env(self, home):
        return {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
                "BRAIN_OFFLINE": "true"}

    def run_brain(self, home, *args, ok=True):
        proc = subprocess.run([BRAIN, *map(str, args)], env=self.env(home),
                              capture_output=True, text=True, timeout=30)
        self.transcript.append(proc.stdout + proc.stderr)
        if ok:
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        else:
            self.assertNotEqual(proc.returncode, 0, proc.stdout)
        return json.loads(proc.stdout)

    def team(self, *args, ok=True):
        return self.run_brain(self.source, "team", *args, ok=ok)

    def seed(self):
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(self.repo), "-c", "user.name=Synthetic",
                        "-c", "user.email=synthetic@example.invalid", "commit", "--allow-empty",
                        "-qm", "Build baseline"], check=True, capture_output=True)
        self.run_brain(self.source, "init")
        now = int(time.time()) - 10
        with sqlite3.connect(self.db) as c:
            c.execute("INSERT OR IGNORE INTO agents(id,created_at) VALUES('fixture',?)", (now,))
            c.execute("INSERT OR IGNORE INTO spaces(id,kind,label,created_by_agent_id,created_at) "
                      "VALUES('default','project','fixture','fixture',?)", (now,))
            c.execute("INSERT INTO sessions(id,space_id,agent_id,started_at) VALUES(?,?,?,?)",
                      ("/profile/raw-session", "default", "fixture", now))
            def node(identity, what, kind="error", project=None, distilled=True, status="confirmed", why=None):
                metadata = {"distilled_fact": distilled, "kind": kind, "what": what,
                            "why": why if why is not None else "Observed build problem", "status": status,
                            "project": project or str(self.repo), "evidence": [1],
                            "archive_session": "/profile/raw-session", "raw_session": self.raw,
                            "tool_output": self.tool, "status_evidence": self.raw,
                            "unknown": {"secret": self.secret, "cwd": self.absolute}}
                c.execute("INSERT INTO nodes(id,space_id,created_by_agent_id,created_in_session_id,"
                          "created_at,last_accessed) VALUES(?,'default','fixture',?, ?,?)",
                          (identity, "/profile/raw-session", now, now))
                c.execute("INSERT INTO node_versions(node_id,space_id,revision,kind,label,body,metadata,"
                          "valid_from,recorded_at,agent_id,session_id) VALUES(?,'default',1,'error',?,?,?,?,?,'fixture',?)",
                          (identity, self.raw, self.tool, json.dumps(metadata), now, now, "/profile/raw-session"))
            node("error-one", "Build issue " + self.secret)
            node("decision-one", "Build decision", kind="decision", status="failed")
            node("plan-one", "Build plan", kind="plan", status="active")
            node("dead-one", "Build dead end", kind="dead_end", status="abandoned")
            node("finding-one", "Build finding", kind="finding", status="confirmed")
            node("absolute-trap", self.absolute)
            node("raw-trap", self.raw, distilled=False)
            node("wont-fix-trap", "Unexplained decision", status="wont_fix", why="")
            outside = self.base / "outside"
            outside.mkdir()
            (self.repo / "external").symlink_to(outside, target_is_directory=True)
            node("symlink-project", "SYMLINK_EXTERNAL_TRAP", project=str(self.repo / "external"))
            node("symlink-path", "external/file")
            c.execute("INSERT INTO edges(id,space_id,from_node_id,to_node_id,created_by_agent_id,"
                      "created_in_session_id,created_at,last_accessed) VALUES('link-one','default',"
                      "'decision-one','error-one','fixture','/profile/raw-session',?,?)", (now, now))
            c.execute("INSERT INTO edge_versions(edge_id,space_id,revision,relation,weight,confidence,metadata,valid_from,"
                      "recorded_at,agent_id,session_id) VALUES('link-one','default',1,'caused_by',1.0,1.0,"
                      "'{\"distilled\":true}',?,?,'fixture','/profile/raw-session')", (now, now))
        first = self.run_brain(self.source, "work", "add", "proj", "Build task", "--detail",
                               self.raw + self.tool + self.secret + self.absolute)
        self.task_id = first["task"]["id"]
        second = self.run_brain(self.source, "work", "add", "proj", "Build dependency",
                                "--depends-on", self.task_id)
        self.run_brain(self.source, "work", "claim", self.task_id)
        with sqlite3.connect(self.source / ".brain/coordination/coord.sqlite") as c:
            c.execute("UPDATE claims SET expires_at=? WHERE task_id=?", (now - 1, self.task_id))
        self.run_brain(self.source, "work", "add", "other-project", "OTHER_PROJECT_TRAP")
        return second

    def configure(self):
        return self.team("configure", "--machine", self.machine, "--project", "proj", "--root", self.repo,
                         "--team-space", "team.proj", "--recipient", "boss:lead")

    def serve(self):
        self.run_brain(self.source, "net", "init", "--state-dir", self.state, "--name", "localhost")
        self.cred = self.coordinator / f"reader-{self.machine}.credential"
        enrolled = self.run_brain(self.source, "net", "enroll", "--state-dir", self.state,
                                  "--machine", "boss", "--agent-id", "lead", "--role", "reader",
                                  "--grant-space", "team.proj", "--credential-out", self.cred)
        self.token_id = enrolled["token_id"]
        self.run_brain(self.source, "net", "share", "grant", "--state-dir", self.state,
                       "--machine", "boss", "--agent-id", "lead", "--space", "team.proj")
        self.server = subprocess.Popen([BRAIN, "net", "serve", "--state-dir", str(self.state),
                                       "--cert", str(self.state / "tls/server.pem"),
                                       "--key", str(self.state / "tls/server.key"), "--port", "0"],
                                      env=self.env(self.source), stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        ready = json.loads(self.server.stdout.readline())
        self.assertEqual(ready["status"], "serving", ready)
        self.assertTrue(ready["bind"].startswith("127.0.0.1:"))
        self.dial = ready["bind"]
        origin = self.team("manifest")["manifests"][0]["manifest"]["origin"]
        self.run_brain(self.coordinator, "team", "peer", "--machine", self.machine, "--origin", origin,
                       "--project", "proj", "--public-key", self.team("manifest")["signing_public_key"], "--team-space", "team.proj", "--dial", self.dial,
                       "--server-name", "localhost", "--ca", self.state / "tls/ca.pem",
                       "--credential", self.cred)

    def ask(self, machines="7,missing", **kw):
        return self.run_brain(self.coordinator, "team", "ask", "Build", "--machines", machines, **kw)

    def test_complete_flow_traps_revocation_offline_and_fail_closed_audit(self):
        before = self.team("manifest")
        self.assertFalse(before["enabled"])
        self.assertFalse(self.state.exists(), "read-only default creates no files")
        self.seed()
        configured = self.configure()
        self.assertFalse(configured["manifest"]["enabled"])
        self.team("publish", ok=False)
        self.team("enable")
        published = self.team("publish")
        self.assertEqual(published["slice"]["coverage"], "incomplete")
        self.assertGreaterEqual(published["slice"]["rejected"], 3)
        repeated = self.team("publish")
        self.assertTrue(repeated["unchanged"])
        self.assertEqual(repeated["slice"]["snapshot_id"], published["slice"]["snapshot_id"])
        facts = self.run_brain(self.source, "net", "share", "facts", "--state-dir", self.state,
                               "--space", "team.proj")
        payloads = [f["payload"] for f in facts["facts"] if f["payload"]["schema"] == "brain-team-fact/1"]
        serialized = json.dumps(payloads)
        for trap in [self.raw, self.tool, self.secret, self.absolute, "/profile/raw-session",
                     "SYMLINK_EXTERNAL_TRAP", "OTHER_PROJECT_TRAP", "synthetic@example.invalid"]:
            self.assertNotIn(trap, serialized)
        classes = {f["content"]["class"] for f in payloads}
        self.assertEqual(classes, {"task", "plan", "error", "decision", "dead_end", "finding",
                                   "link", "commit", "participation", "session"})
        self.assertTrue(any("[REDACTED]" in json.dumps(f) for f in payloads))
        task = next(f for f in payloads if f["content"].get("title_summary") == "Build task")
        self.assertEqual(task["content"]["state"], "queued")
        for f in payloads:
            self.assertIsNone(f["model_ref"])
            self.assertEqual(f["model_evidence"], "unknown")
        # Generic free JSON, claim or agent cannot bypass the reserved team boundary.
        self.run_brain(self.source, "net", "share", "publish", "--state-dir", self.state,
                       "--space", "team.proj", "--fact-id", "injected", "--payload", "{}",
                       "--claim", self.raw, "--claimed-agent", "fake", ok=False)
        self.serve()
        answer = self.ask()
        self.assertTrue(answer["hits"], answer)
        self.assertTrue(all("машина 7; класс" in line for line in answer["lines"][:-2]))
        self.assertTrue(any("нет среза" in line for line in answer["lines"]))
        self.assertTrue(all(h["received_from_peer"] == "7" for h in answer["hits"]))
        for trap in [self.raw, self.tool, self.secret, self.absolute, "/profile/raw-session"]:
            self.assertNotIn(trap, json.dumps(answer))
        # Natural manager questions must retain the same privacy and attribution boundary.
        for query, classes in [
                ("Какие ошибки сейчас открыты?", {"error"}),
                ("Which decisions failed and why?", {"decision", "dead_end"}),
                ("Что в планах на следующий спринт?", {"plan"})]:
            natural = self.run_brain(self.coordinator, "team", "ask", query, "--machines", "7")
            self.assertEqual({h["fact"]["content"]["class"] for h in natural["hits"]}, classes)
            self.assertTrue(all(h["received_from_peer"] == "7" for h in natural["hits"]))
            for trap in [self.raw, self.tool, self.secret, self.absolute, "/profile/raw-session"]:
                self.assertNotIn(trap, json.dumps(natural))
        for query in ["Какие ошибки открыты по проекту forbidden?",
                      "Open errors on machine forbidden", "Open errors over the past week impossibleword"]:
            natural = self.run_brain(self.coordinator, "team", "ask", query, "--machines", "7")
            self.assertFalse(natural["hits"])
        # Every processing outcome is journaled on both ends, without question text.
        for state in [self.state, self.coordstate]:
            with sqlite3.connect(state / "share.sqlite") as c:
                logs = [json.loads(row[0]) for row in c.execute("SELECT document FROM team_audit")]
            self.assertTrue(logs)
            self.assertTrue(any(row["request_id"] == answer["request_id"] for row in logs))
            self.assertTrue(all("Build" not in json.dumps(row) for row in logs))
        # Per-field local policy is closed and reprojected before serving.
        self.team("disable", "--class", "plan", "--field", "why_summary")
        self.assertFalse(self.ask("7")["hits"])
        self.team("publish")
        field_filtered = self.ask("7")
        plans = [h["fact"] for h in field_filtered["hits"] if h["fact"]["content"]["class"] == "plan"]
        self.assertTrue(plans)
        self.assertEqual(plans[0]["content"]["why_summary"], "")
        self.team("disable", "--class", "plan", "--field", "unknown_field", ok=False)
        self.team("enable", "--class", "plan", "--field", "why_summary")
        self.team("publish")
        # Coordinator cache is not used for content when source denies.
        self.team("disable", "--class", "error")
        denied = self.ask("7")
        self.assertFalse(denied["hits"])
        self.assertEqual(denied["machines"][0]["status"], "denied")
        # Reproject: the event window never serves old error or session text.
        self.team("publish")
        filtered = self.ask("7")
        self.assertTrue(filtered["hits"])
        self.assertNotIn("error", {h["fact"]["content"]["class"] for h in filtered["hits"]})
        self.assertNotIn("Build issue", json.dumps(filtered))
        # Wrong digest/partial generation cannot replace or answer from the replica.
        with sqlite3.connect(self.state / "share.sqlite") as c:
            seq, digest = c.execute("SELECT seq,event_digest FROM events WHERE space='team.proj' ORDER BY seq DESC LIMIT 1").fetchone()
            c.execute("UPDATE events SET event_digest=? WHERE seq=? AND space='team.proj'", ("0" * 64, seq))
        corrupted = self.ask("7")
        self.assertFalse(corrupted["hits"])
        with sqlite3.connect(self.state / "share.sqlite") as c:
            c.execute("UPDATE events SET event_digest=? WHERE seq=? AND space='team.proj'", (digest, seq))
        self.assertTrue(self.ask("7")["hits"])
        # Origin/space/project are pinned locally, not accepted from wire payload.
        with sqlite3.connect(self.coordstate / "share.sqlite") as c:
            original_peer = c.execute("SELECT document FROM team_peers WHERE machine='7'").fetchone()[0]
            peer = json.loads(original_peer)
            peer["origin"] = "install-spoofed"
            c.execute("UPDATE team_peers SET document=? WHERE machine='7'", (json.dumps(peer),))
        self.assertFalse(self.ask("7")["hits"])
        with sqlite3.connect(self.coordstate / "share.sqlite") as c:
            c.execute("UPDATE team_peers SET document=? WHERE machine='7'", (original_peer,))
        # Before the first observed generation, history is explicitly unknown.
        history = self.run_brain(self.coordinator, "team", "ask", "Build", "--machines", "7", "--recorded-at", "1")
        self.assertFalse(history["hits"])
        self.assertEqual(history["machines"][0]["status"], "history_unknown")
        # Whole-machine local opt-out dominates manually re-enabled host space/grant.
        self.team("disable")
        self.run_brain(self.source, "net", "share", "enable", "--state-dir", self.state,
                       "--space", "team.proj")
        self.assertFalse(self.ask("7")["hits"])
        self.team("enable")
        self.team("publish")
        # Audit storage failure on source denies even an otherwise valid request.
        with sqlite3.connect(self.state / "share.sqlite") as c:
            c.execute("CREATE TRIGGER deny_audit BEFORE INSERT ON team_audit "
                      "BEGIN SELECT RAISE(ABORT,'synthetic audit unavailable'); END")
        blocked = self.ask("7")
        self.assertFalse(blocked["hits"])
        with sqlite3.connect(self.state / "share.sqlite") as c:
            c.execute("DROP TRIGGER deny_audit")
        self.assertTrue(self.ask("7")["hits"])
        # Audit storage failure at coordinator refuses the complete CLI operation.
        with sqlite3.connect(self.coordstate / "share.sqlite") as c:
            c.execute("CREATE TRIGGER deny_audit BEFORE INSERT ON team_audit "
                      "BEGIN SELECT RAISE(ABORT,'synthetic audit unavailable'); END")
        self.ask("7", ok=False)
        with sqlite3.connect(self.coordstate / "share.sqlite") as c:
            c.execute("DROP TRIGGER deny_audit")
        # Offline is explicit, including the last snapshot age, with no cached hits.
        self.server.stdin.close()
        self.server.wait(timeout=15)
        self.server.stdout.close()
        self.server.stderr.close()
        self.server = None
        offline = self.ask("7")
        self.assertFalse(offline["hits"])
        self.assertEqual(offline["machines"][0]["status"], "offline")
        self.assertIsInstance(offline["machines"][0]["age_seconds"], int)
        self.assertTrue(any("нет ответа" in line for line in offline["lines"]))


if __name__ == "__main__":
    unittest.main()
