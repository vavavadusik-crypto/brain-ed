"""M3: real loopback TLS with disposable homes; no installed Brain or live stand."""
import json
import http.client
import socket
import ssl
import os
from pathlib import Path
import sqlite3
import unittest
import sys

# Runs both as `python -m tests.test_team_security` and as a plain script.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from tests import test_team_roles as roles  # noqa: E402


@unittest.skipUnless(roles.BRAIN, "set BRAIN_PROBE_BIN or BRAIN_BIN")
class TeamSecurity(unittest.TestCase):
    setUp = roles.TeamRoles.setUp
    home = roles.TeamRoles.home
    cli = roles.TeamRoles.cli
    invite = roles.TeamRoles.invite
    member = roles.TeamRoles.member
    request = roles.TeamRoles.request
    ask = roles.TeamRoles.ask

    def source_request(self, worker, token, space, rights=None):
        with sqlite3.connect(self.admin / ".brain/net/auth.sqlite") as db:
            source = json.loads(db.execute("SELECT m.source FROM team_members m JOIN principals p ON p.id=m.principal_id WHERE p.machine_id=?", (worker.name,)).fetchone()[0])
        context = ssl.create_default_context(cafile=str(worker / ".brain/net/tls/ca.pem"))
        context.verify_flags &= ~ssl.VERIFY_X509_STRICT
        host, port = source["dial"].rsplit(":", 1)
        arguments = {"space": space}
        if rights is not None:
            arguments["rights"] = rights
        body = json.dumps({"protocol": 1, "tool": "share_pull", "arguments": arguments}).encode()
        with socket.create_connection((host, int(port)), timeout=5) as sock:
            with context.wrap_socket(sock, server_hostname="localhost") as tls:
                tls.sendall((f"POST /v1/call HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n"
                             f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n").encode() + body)
                response = http.client.HTTPResponse(tls)
                response.begin()
                return response.status, json.loads(response.read())

    def test_source_rejects_missing_token_and_foreign_project_independently(self):
        worker_a, _ = self.member("worker-a", project="a")
        worker_b, _ = self.member("worker-b", project="b")
        self.member("manager-a", "manager", project="a")
        with sqlite3.connect(self.admin / ".brain/net/auth.sqlite") as db:
            rights = db.execute("SELECT s.rights FROM team_security s JOIN principals p ON p.id=s.principal_id WHERE p.machine_id='manager-a'").fetchone()[0]
        token_a = (worker_a / ".brain/net/team-source.credential").read_text().strip()
        token_b = (worker_b / ".brain/net/team-source.credential").read_text().strip()
        self.assertEqual(self.source_request(worker_a, token_a, "team.a")[0], 403)
        self.assertEqual(self.source_request(worker_b, token_b, "team.b", rights)[0], 403)
        self.assertEqual(self.source_request(worker_a, token_a, "team.a", rights)[0], 200)

    def test_signed_slice_key_permissions_and_replica_tamper(self):
        worker, _ = self.member("worker-one")
        manager, _ = self.member("manager-one", "manager")
        answer = self.ask(manager, ["worker-one"])
        self.assertTrue(answer["hits"], answer)
        for home in (worker, manager, self.admin):
            key = home / ".brain/net/team-ed25519.key"
            self.assertEqual(key.stat().st_mode & 0o777, 0o600)
            self.assertEqual(key.stat().st_size, 32)
            self.assertNotIn(key.read_bytes().hex(), json.dumps(answer))
        manifest = self.cli(worker, "team", "manifest")
        sliced = manifest["manifests"][0]["last_slice"]
        self.assertEqual(sliced["integrity"], "tls+blake3+ed25519")
        self.assertTrue(sliced["signature"])
        with sqlite3.connect(manager / ".brain/net/share.sqlite") as db:
            replica = json.loads(db.execute("SELECT document FROM team_replicas").fetchone()[0])
            replica["facts"][0]["valid_from"] += 1
            db.execute("UPDATE team_replicas SET document=?", (json.dumps(replica),))
        denied = self.ask(manager, ["worker-one"])
        self.assertEqual(denied["hits"], [])
        self.assertEqual(denied["machines"][0]["status"], "signature_failed")
        self.assertIn("подпись не прошла", "\n".join(denied["lines"]))

    def test_rekey_journal_new_publish_and_old_snapshot_replay(self):
        worker, _ = self.member("worker-one")
        manager, _ = self.member("manager-one", "manager")
        self.assertTrue(self.ask(manager, ["worker-one"])["hits"])
        state = worker / ".brain/net"
        old_key = (state / "team-ed25519.key").read_bytes()
        backup = self.base / "old-source.sqlite"
        with sqlite3.connect(state / "share.sqlite") as live, sqlite3.connect(backup) as saved:
            live.backup(saved)
        self.cli(worker, "team", "rekey")
        self.assertNotEqual((state / "team-ed25519.key").read_bytes(), old_key)
        self.assertFalse(self.ask(manager, ["worker-one"])["hits"])
        self.cli(worker, "team", "publish")
        self.assertTrue(self.ask(manager, ["worker-one"])["hits"])
        with sqlite3.connect(self.admin / ".brain/net/auth.sqlite") as db:
            audit = db.execute("SELECT old_key,new_key,epoch FROM team_key_audit WHERE old_key IS NOT NULL").fetchall()
        self.assertEqual(len(audit), 1)
        self.assertEqual(audit[0][2], 2)
        with sqlite3.connect(backup) as saved, sqlite3.connect(state / "share.sqlite") as live:
            saved.backup(live)
        replay = self.ask(manager, ["worker-one"])
        self.assertEqual(replay["hits"], [])
        self.assertEqual(replay["machines"][0]["status"], "signature_failed")

    def test_manager_multiple_projects_and_cross_project_refusal(self):
        self.member("worker-a", project="a")
        self.member("worker-b", project="b")
        invitation = self.base / "multi.invite.json"
        self.cli(self.admin, "team", "invite", "--role", "manager", "--name", "multi-lead",
                 "--projects", "a,b", "--expires", "30d", "--out", invitation)
        manager = self.home("multi-lead")
        self.cli(manager, "team", "accept", invitation)
        # The coordinator grants both scopes and pins both worker keys.
        who = self.cli(manager, "team", "who")
        self.assertEqual(who["status"], "ok")
        for project in ("a", "b"):
            status, body = self.request(manager, "team_pull", {"space": "team." + project,
                                        "source_name": "worker-" + project})
            self.assertEqual(status, 200, body)
        restricted, _ = self.member("restricted-lead", "manager", project="a")
        self.assertEqual(self.request(restricted, "team_pull", {"space": "team.b",
                         "source_name": "worker-b"})[0], 403)

    def test_revoked_worker_closes_existing_manager_cache(self):
        self.member("worker-one")
        manager, _ = self.member("manager-one", "manager")
        self.assertTrue(self.ask(manager, ["worker-one"])["hits"])
        self.cli(self.admin, "team", "remove", "worker-one")
        refused = self.ask(manager, ["worker-one"])
        self.assertEqual(refused["hits"], [])
        self.assertEqual(refused["machines"][0]["status"], "denied")


if __name__ == "__main__":
    unittest.main()
