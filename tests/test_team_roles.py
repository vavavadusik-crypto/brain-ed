"""M2: real TLS processes, synthetic homes only; never use the installed brain."""
import http.client
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import ssl
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[1]
BRAIN = os.environ.get("BRAIN_PROBE_BIN") or os.environ.get("BRAIN_BIN")
spec = importlib.util.spec_from_file_location("team_demo_roles", REPO / "tools/team-demo.py")
demo_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(demo_module)


@unittest.skipUnless(BRAIN, "set BRAIN_PROBE_BIN or BRAIN_BIN")
class TeamRoles(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="brain-team-roles-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.demo = demo_module.Demo(str(Path(BRAIN).resolve()), self.base)
        self.addCleanup(self.demo.close)
        self.admin = self.home("admin")
        self.demo.run(self.admin, "net", "init", "--name", "localhost")
        _, self.address = self.demo.serve(self.admin)

    def home(self, name):
        home = self.base / name
        home.mkdir(exist_ok=True)
        return home

    def cli(self, home, *args, ok=True):
        p = subprocess.run([self.demo.brain, "--offline", *map(str, args)],
                           env=self.demo.env(home), cwd=self.base, input="",
                           capture_output=True, text=True, timeout=15)
        self.assertEqual(p.returncode == 0, ok, p.stdout + p.stderr)
        self.assertNotIn("brc1.", p.stdout + p.stderr)
        return json.loads(p.stdout) if ok else p

    def invite(self, name, role="worker", project="checkout"):
        invitation = self.base / f"{name}.invite.json"
        result = self.cli(self.admin, "team", "invite", "--role", role, "--name", name,
                          "--project", project, "--out", invitation)
        self.assertEqual(invitation.stat().st_mode & 0o777, 0o600)
        self.assertEqual(result["role"], role)
        return invitation

    def member(self, name, role="worker", publish=True, project="checkout"):
        home = self.home(name)
        invitation = self.invite(name, role, project)
        args = []
        if role == "worker":
            root = self.demo.seed(home, 1)
            self.demo.run(home, "net", "init", "--name", "localhost")
            self.demo.serve(home)
            args = ["--root", root] + (["--publish"] if publish else [])
        self.cli(home, "team", "accept", invitation, *args)
        return home, invitation

    def request(self, home, tool, arguments, token=None):
        client = home / ".brain/net/client"
        if token is None:
            token = (client / "credential.pem").read_text().strip()
        ca = self.admin / ".brain/net/tls/ca.pem"
        context = ssl.create_default_context(cafile=str(ca))
        # Same compatibility setting as test_relay: rcgen's local CA omits AKI.
        # Signature, CA pinning, validity and hostname checks remain enabled.
        context.verify_flags &= ~ssl.VERIFY_X509_STRICT
        host, port = self.address.rsplit(":", 1)
        with socket.create_connection((host, int(port)), timeout=5) as sock:
            with context.wrap_socket(sock, server_hostname="localhost") as tls:
                body = json.dumps({"protocol": 1, "tool": tool, "arguments": arguments}).encode()
                tls.sendall((f"POST /v1/call HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n"
                             f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n").encode() + body)
                response = http.client.HTTPResponse(tls)
                response.begin()
                return response.status, json.loads(response.read())

    def ask(self, home, machines):
        return self.cli(home, "team", "ask", "Какие ошибки сейчас открыты?",
                        "--machines", ",".join(machines))

    def test_admin_manager_three_workers_revocation_and_opt_out(self):
        manager, _ = self.member("manager", "manager")
        workers = [self.member(f"worker-{n}")[0] for n in range(1, 4)]
        names = [w.name for w in workers]
        answer = self.ask(manager, names)
        self.assertEqual({hit["received_from_peer"] for hit in answer["hits"]}, set(names))
        roster = self.cli(manager, "team", "who")
        self.assertEqual({m["role"] for m in roster["members"]}, {"manager", "worker"})
        self.assertEqual(len(roster["members"]), 4)
        self.assertNotIn("127.0.0.1", json.dumps(roster))
        self.assertNotIn("credential", json.dumps(roster))
        self.assertTrue(all(m["freshness"] == "fresh" for m in roster["members"] if m["role"] == "worker"))
        self.cli(workers[0], "team", "disable")
        denied = self.ask(manager, [names[0]])
        self.assertEqual(denied["hits"], [])
        self.assertEqual(denied["machines"][0]["status"], "denied")
        self.cli(self.admin, "team", "remove", "manager")
        self.assertEqual(self.request(manager, "team_pull", {"space": "team.checkout", "source_name": names[1]})[0], 401)
        self.cli(manager, "team", "ask", "ошибки", "--machines", names[1], ok=False)
        self.cli(manager, "team", "who", ok=False)

    def test_worker_cannot_read_other_slices_manager_cannot_publish(self):
        worker, _ = self.member("worker-1")
        self.member("worker-2")
        manager, _ = self.member("manager", "manager")
        self.assertEqual(self.request(worker, "team_pull", {"space": "team.checkout", "source_name": "worker-2"})[0], 403)
        self.assertEqual(self.request(worker, "share_pull", {"space": "team.checkout"})[0], 403)
        self.assertEqual(self.request(manager, "team_register", {"space": "team.checkout", "source": {}})[0], 403)
        self.assertEqual(self.request(manager, "work_add", {"space": "team.checkout", "project": "checkout", "title": "must fail"})[0], 403)
        self.cli(manager, "team", "publish", ok=False)
        self.cli(manager, "team", "configure", "--machine", "manager", "--project", "checkout",
                 "--root", manager, "--team-space", "team.checkout", "--recipient", "other:team", ok=False)
        self.cli(worker, "team", "invite", "--role", "manager", "--name", "intruder", ok=False)
        self.cli(manager, "team", "remove", "worker-1", ok=False)

    def test_single_use_invitation_is_checked_by_server_and_pending_cannot_read(self):
        invitation = self.invite("manager", "manager")
        token = json.loads(invitation.read_text())["connection"]["credential"]
        self.assertEqual(self.request(self.admin, "work_whoami", {}, token=token)[0], 403)
        self.cli(self.home("manager"), "team", "accept", invitation)
        self.assertEqual(self.request(self.admin, "team_accept", {"space": "team.checkout"}, token=token)[0], 401)
        self.cli(self.home("replay"), "team", "accept", invitation, ok=False)

    def test_expired_invitation_server_checks_even_with_forged_file_expiry(self):
        invitation = self.invite("manager", "manager")
        document = json.loads(invitation.read_text())
        token = document["connection"]["credential"]
        with sqlite3.connect(self.admin / ".brain/net/auth.sqlite") as conn:
            conn.execute("UPDATE credentials SET not_after=?", (int(time.time()) - 1,))
        # File still advertises a future expiry: only the server's clock matters.
        self.assertGreater(document["connection"]["expires_at"], int(time.time()))
        self.assertEqual(self.request(self.admin, "team_accept", {"space": "team.checkout"}, token=token)[0], 401)
        self.cli(self.home("manager"), "team", "accept", invitation, ok=False)

    def test_no_publish_defaults_off_and_later_explicit_publish_registers_source(self):
        manager, _ = self.member("manager", "manager")
        worker = self.home("worker-1")
        root = self.demo.seed(worker, 1)
        invitation = self.invite("worker-1")
        self.cli(worker, "team", "accept", invitation, "--root", root)
        manifest = self.cli(worker, "team", "manifest")["manifests"][0]
        self.assertFalse(manifest["manifest"]["enabled"])
        self.assertIsNone(manifest["last_slice"])
        self.demo.serve(worker)
        self.cli(worker, "team", "enable")
        self.cli(worker, "team", "publish")
        self.assertTrue(self.ask(manager, ["worker-1"])["hits"])
        self.cli(self.admin, "team", "remove", "worker-1")
        self.assertEqual(self.ask(manager, ["worker-1"])["hits"], [])

    def test_project_boundary_and_manager_grant_revocation(self):
        manager, _ = self.member("manager", "manager", project="alpha")
        worker, _ = self.member("worker-1", project="beta")
        self.assertEqual(self.request(manager, "team_pull", {"space": "team.beta", "source_name": worker.name})[0], 403)
        self.assertEqual(self.cli(manager, "team", "peers")["peers"], [])
        self.cli(self.admin, "net", "share", "ungrant", "--state-dir", self.admin / ".brain/net",
                 "--machine", "manager", "--agent-id", "team", "--space", "team.alpha")
        self.assertEqual(self.request(manager, "team_pull", {"space": "team.alpha", "source_name": worker.name})[0], 403)

    def test_manager_mcp_discovery_before_workers_and_denial_after_revoke(self):
        manager, _ = self.member("manager", "manager")
        def mcp(method, params=None):
            request = {"jsonrpc": "2.0", "id": 1, "method": method}
            if params is not None:
                request["params"] = params
            p = subprocess.run([self.demo.brain, "--offline", "mcp"],
                               env=self.demo.env(manager), input=json.dumps(request)+"\n",
                               capture_output=True, text=True, timeout=15)
            self.assertEqual(p.returncode, 0, p.stderr)
            self.assertNotIn("brc1.", p.stdout+p.stderr)
            return json.loads(p.stdout)["result"]
        self.assertIn("team_ask", {t["name"] for t in mcp("tools/list")["tools"]})
        self.member("worker-1")
        params = {"name": "team_ask", "arguments": {"query": "Какие ошибки сейчас открыты?", "machines": ["worker-1"]}}
        result = mcp("tools/call", params)
        self.assertFalse(result["isError"])
        self.assertTrue(result["structuredContent"]["hits"])
        self.cli(self.admin, "team", "remove", "manager")
        result = mcp("tools/call", params)
        self.assertTrue(result["isError"])
        self.assertNotIn("hits", result["structuredContent"])

    def test_wrong_role_file_does_not_consume_invitation_and_worker_retry_is_off(self):
        invitation = self.invite("worker-1")
        document = json.loads(invitation.read_text())
        document["role"] = "manager"
        invitation.write_text(json.dumps(document))
        worker = self.home("worker-1")
        self.cli(worker, "team", "accept", invitation, ok=False)
        document["role"] = "worker"
        invitation.write_text(json.dumps(document))
        self.cli(worker, "team", "accept", invitation)
        self.assertFalse(self.cli(worker, "team", "manifest")["manifests"][0]["manifest"]["enabled"])

    def test_worker_failed_route_approval_can_retry_without_publishing(self):
        invitation = self.invite("worker-1")
        worker = self.home("worker-1")
        self.demo.run(worker, "net", "init", "--name", "localhost")
        self.demo.serve(worker)
        self.cli(worker, "team", "accept", invitation, "--allow-lan-source", ok=False)
        self.assertFalse((worker / ".brain/net/team-source.credential").exists())
        self.cli(worker, "team", "accept", invitation)
        self.assertFalse(self.cli(worker, "team", "manifest")["manifests"][0]["manifest"]["enabled"])

    def test_interactive_negative_confirmation_leaves_worker_off(self):
        import pty
        invitation = self.invite("worker-1")
        worker = self.home("worker-1")
        master, slave = pty.openpty()
        try:
            proc = subprocess.Popen([self.demo.brain, "--offline", "team", "accept", str(invitation)],
                                    env=self.demo.env(worker), cwd=self.base, stdin=slave,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            os.close(slave)
            slave = None
            os.write(master, b"n\n")
            stdout, stderr = proc.communicate(timeout=15)
            self.assertEqual(proc.returncode, 0, stderr)
            self.assertIn("[y/N]", stderr)
            self.assertFalse(json.loads(stdout)["publishing"])
        finally:
            os.close(master)
            if slave is not None:
                os.close(slave)

    def test_relative_state_directory_stores_absolute_peer_credentials(self):
        self.member("worker-1")
        invitation = self.invite("manager", "manager")
        manager = self.home("manager")
        self.cli(manager, "team", "accept", invitation, "--state-dir", "manager-state")
        answer = self.cli(manager, "team", "ask", "Какие ошибки сейчас открыты?",
                          "--machines", "worker-1", "--state-dir", "manager-state")
        self.assertTrue(answer["hits"])

    def test_existing_public_coordinator_auth_state_is_refused_before_redemption(self):
        invitation = self.invite("manager", "manager")
        database = self.admin / ".brain/net/auth.sqlite"
        database.chmod(0o644)
        manager = self.home("manager")
        self.cli(manager, "team", "accept", invitation, ok=False)
        self.assertFalse((manager / ".brain/net/client").exists())
        database.chmod(0o600)
        self.cli(manager, "team", "accept", invitation)

    def test_invitation_permissions_symlink_and_noninteractive_manager_publish(self):
        invitation = self.invite("manager", "manager")
        home = self.home("manager")
        invitation.chmod(0o644)
        self.cli(home, "team", "accept", invitation, ok=False)
        invitation.chmod(0o600)
        symlink = self.base / "link.json"
        symlink.symlink_to(invitation)
        self.cli(home, "team", "accept", symlink, ok=False)
        self.cli(home, "team", "accept", invitation, "--publish", ok=False)
        self.cli(home, "team", "accept", invitation)


if __name__ == "__main__":
    unittest.main()
