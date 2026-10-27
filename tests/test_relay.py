"""Real loopback relay/coordinator/client processes; disposable state, no models."""
import json
import os
from pathlib import Path
import selectors
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[1]
BRAIN = os.environ.get("BRAIN_PROBE_BIN") or os.environ.get("BRAIN_BIN")


@unittest.skipUnless(BRAIN, "set BRAIN_PROBE_BIN or BRAIN_BIN")
class RelayProcess(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="brain-relay-process-")
        self.base = Path(self.tmp.name)
        self.processes = []
        self.addCleanup(self.cleanup)
        for name in ("relay", "coord", "client", "fake"):
            (self.base / name).mkdir(mode=0o700)
        for name in ("relay", "coord", "fake"):
            self.run_cli(name, "net", "init", "--state-dir", self.base / name / "net", "--name", "localhost")
        self.relay = self.start("relay", "net", "relay", "serve", "--listen", "127.0.0.1", "--port", "0",
                                "--state-dir", self.base / "relay/net")
        self.address = self.relay[1]["bind"]
        self.reg = self.issue("alpha-coord", "register")
        self.conn = self.issue("alpha-coord", "connect")
        self.run_cli("coord", "net", "enroll", "--state-dir", self.base / "coord/net", "--machine", "client-one",
                     "--agent-id", "test", "--role", "worker", "--grant-space", "demo", "--credential-out", self.base / "credential")
        self.coord = self.start("coord", "net", "serve", "--state-dir", self.base / "coord/net", "--port", "0",
                                *self.route(self.reg))
        self.wait_status()

    def env(self, home):
        return {"HOME": str(self.base / home), "BRAIN_HOME": str(self.base / home / ".brain"),
                "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "BRAIN_OFFLINE": "true"}

    def run_cli(self, home, *args, success=True):
        p = subprocess.run([str(Path(BRAIN).resolve()), "--offline", *map(str, args)], cwd=self.base,
                           env=self.env(home), capture_output=True, text=True, timeout=12)
        if success:
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        return p

    def start(self, home, *args):
        log = (self.base / f"{home}-{len(self.processes)}.log").open("w+")
        p = subprocess.Popen([str(Path(BRAIN).resolve()), "--offline", *map(str, args)], cwd=self.base,
                             env=self.env(home), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
        self.processes.append((p, log))
        with selectors.DefaultSelector() as selector:
            selector.register(p.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(8), "startup timed out")
        line = p.stdout.readline()
        self.assertTrue(line, "startup failed")
        ready = json.loads(line)
        self.assertEqual(ready["status"], "serving", ready)
        return p, ready

    def stop(self, p):
        if p.poll() is None:
            p.stdin.close()
            p.wait(timeout=8)
        self.assertEqual(p.returncode, 0)

    def cleanup(self):
        for p, log in reversed(self.processes):
            if p.poll() is None:
                p.stdin.close()
                try:
                    p.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait(timeout=3)
            p.stdout.close()
            if not p.stdin.closed:
                p.stdin.close()
            log.close()
        self.tmp.cleanup()

    def issue(self, name, role):
        path = self.base / f"{name}-{role}.token"
        p = self.run_cli("relay", "net", "relay", "token", "add", "--state-dir", self.base / "relay/net",
                         "--name", name, "--role", role, "--token-out", path)
        return path, json.loads(p.stdout)["token_id"]

    def route(self, token=None, name="alpha-coord"):
        return ["--via-relay", self.address, "--relay-name", name, "--relay-token-file", (token or self.conn)[0],
                "--relay-ca", self.base / "relay/net/tls/ca.pem", "--relay-server-name", "localhost"]

    def status(self, *extra):
        return self.run_cli("client", "net", "status", "--server-name", "localhost", "--ca", self.base / "coord/net/tls/ca.pem",
                            "--credential", self.base / "credential", *self.route(), *extra, success=False)

    def wait_status(self):
        deadline = time.monotonic() + 12
        while True:
            p = self.status()
            if p.returncode == 0:
                return json.loads(p.stdout)
            self.assertLess(time.monotonic(), deadline, p.stdout + p.stderr)
            time.sleep(0.1)

    def wire(self, token, role, name="alpha-coord"):
        context = ssl.create_default_context(cafile=str(self.base / "relay/net/tls/ca.pem"))
        # rcgen net-init certificates are accepted by the production rustls
        # verifier. Keep CA+hostname verification in this test peer while
        # disabling OpenSSL's additional X.509 strict profile (AKI requirement).
        context.verify_flags &= ~ssl.VERIFY_X509_STRICT
        host, port = self.address.split(":")
        s = context.wrap_socket(socket.create_connection((host, int(port)), timeout=3), server_hostname="localhost")
        s.settimeout(3)
        self.addCleanup(s.close)
        header = json.dumps({"protocol": "brain-relay/1", "role": role, "name": name, "token": token}).encode()
        s.sendall(struct.pack("!H", len(header)) + header)
        return s

    def test_relay_only_status_call_bundle_and_direct_fallback(self):
        result = self.wait_status()
        self.assertEqual(result["principal"]["machine"], "client-one")
        call = self.run_cli("client", "net", "call", "--space", "demo", "--server-name", "localhost", "--ca", self.base / "coord/net/tls/ca.pem",
                            "--credential", self.base / "credential", *self.route(), "board", "--project", "demo", success=False)
        self.assertEqual(call.returncode, 0, call.stdout + call.stderr)
        with socket.socket() as unavailable:
            unavailable.bind(("127.0.0.1", 0))
            dead = f"127.0.0.1:{unavailable.getsockname()[1]}"
            self.assertEqual(self.status("--dial", dead).returncode, 0)
        bundle = self.base / "join.bundle"
        self.run_cli("coord", "net", "enroll", "--state-dir", self.base / "coord/net", "--machine", "joined-client",
                     "--agent-id", "test", "--role", "reader", "--grant-space", "demo", "--credential-out", self.base / "joined.cred",
                     "--bundle-out", bundle, *self.route())
        payload = json.loads(bundle.read_text())
        self.assertEqual(payload["version"], 2)
        self.assertEqual(payload["dial"], [])
        self.run_cli("client", "net", "join", bundle, "--consume")
        joined = self.run_cli("client", "net", "status")
        self.assertEqual(json.loads(joined.stdout)["principal"]["machine"], "joined-client")
        for path in [self.base / "relay/net/relay-auth/auth.sqlite", self.conn[0],
                     self.base / "client/.brain/net/client/relay-token"]:
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        stored = (self.base / "relay/net/relay-auth/auth.sqlite").read_bytes()
        self.assertNotIn(self.conn[0].read_text().strip().encode(), stored)
        # The leader's unrelated net-join relay must not fill a direct peer's
        # complete saved connection. An invalid joined relay CA makes any
        # accidental inheritance observable without contacting that relay.
        (self.base / "client/.brain/net/client/relay-ca.pem").write_text("invalid CA")
        self.run_cli("client", "team", "peer", "--machine", "source-worker", "--origin", "synthetic-origin",
                     "--public-key", "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                     "--project", "demo", "--team-space", "team.demo", "--dial", self.coord[1]["bind"],
                     "--server-name", "localhost", "--ca", self.base / "coord/net/tls/ca.pem",
                     "--credential", self.base / "credential")


    def test_foreign_token_role_and_name_substitution_refused(self):
        foreign = self.issue("other-coord", "register")
        for token, role, name in [(self.conn[0].read_text().strip(), "register", "alpha-coord"),
                                  (foreign[0].read_text().strip(), "register", "alpha-coord"),
                                  ((self.base / "credential").read_text().strip(), "connect", "alpha-coord"),
                                  (self.reg[0].read_text().strip(), "connect", "alpha-coord"),
                                  ("brc1.unknown.unknown", "connect", "alpha-coord")]:
            with self.subTest(role=role, name=name):
                s = self.wire(token, role, name)
                try:
                    self.assertEqual(s.recv(1), b"")
                except (ssl.SSLError, ConnectionResetError):
                    pass
        for p, log in self.processes:
            log.flush()
            text = Path(log.name).read_text()
            self.assertNotIn(self.conn[0].read_text().strip(), text)
            self.assertNotIn("work_whoami", text)

    def test_revocation_closes_active_tunnel_and_blocks_new_clients(self):
        # Dedicated name avoids competing with the coordinator registration pool.
        reg = self.issue("active-test", "register")
        conn = self.issue("active-test", "connect")
        r = self.wire(reg[0].read_text().strip(), "register", "active-test")
        self.assertEqual(r.recv(1), b"R")
        c = self.wire(conn[0].read_text().strip(), "connect", "active-test")
        self.assertEqual(c.recv(1), b"R")
        self.assertEqual(c.recv(1), b"G")
        self.assertEqual(r.recv(1), b"G")
        self.run_cli("relay", "net", "relay", "token", "revoke", "--state-dir", self.base / "relay/net", "--token-id", conn[1])
        started = time.monotonic()
        self.assertEqual(c.recv(1), b"")
        self.assertLess(time.monotonic() - started, 2)
        self.run_cli("relay", "net", "relay", "token", "revoke", "--state-dir", self.base / "relay/net", "--token-id", self.conn[1])
        self.assertNotEqual(self.status().returncode, 0)

    def test_unauthenticated_connection_cap_and_auth_deadline(self):
        limited = self.start("relay", "net", "relay", "serve", "--listen", "127.0.0.1", "--port", "0",
                             "--state-dir", self.base / "relay/net", "--max-connections", "2", "--idle-secs", "1",
                             "--max-session-secs", "1")
        host, port = limited[1]["bind"].split(":")
        sockets = [socket.create_connection((host, int(port)), timeout=2) for _ in range(2)]
        for s in sockets:
            self.addCleanup(s.close)
        time.sleep(0.15)
        third = socket.create_connection((host, int(port)), timeout=2)
        self.addCleanup(third.close)
        self.assertEqual(third.recv(1), b"")
        # An unauthenticated peer cannot retain a slot by keeping TCP open.
        started = time.monotonic()
        for s in sockets:
            s.settimeout(4)
            self.assertEqual(s.recv(1), b"")
        self.assertLess(time.monotonic() - started, 4)
        old_address = self.address
        try:
            self.address = limited[1]["bind"]
            r = self.wire(self.reg[0].read_text().strip(), "register")
            self.assertEqual(r.recv(1), b"R")
            started = time.monotonic()
            self.assertEqual(r.recv(1), b"")
            self.assertLess(time.monotonic() - started, 2)
        finally:
            self.address = old_address

    def test_reconnect_after_relay_restart(self):
        self.stop(self.relay[0])
        self.start("relay", "net", "relay", "serve", "--listen", "127.0.0.1", "--port", self.address.split(":")[1],
                   "--state-dir", self.base / "relay/net")
        self.wait_status()

    def test_coordinator_ca_and_name_remain_pinned_through_malicious_route(self):
        reg = self.issue("evil-coord", "register")
        conn = self.issue("evil-coord", "connect")
        self.start("fake", "net", "serve", "--state-dir", self.base / "fake/net", "--port", "0", *self.route(reg, "evil-coord"))
        deadline = time.monotonic() + 8
        while True:
            p = self.run_cli("client", "net", "status", "--server-name", "localhost", "--ca", self.base / "coord/net/tls/ca.pem",
                             "--credential", self.base / "credential", *self.route(conn, "evil-coord"), success=False)
            if p.returncode == 5:
                break
            self.assertLess(time.monotonic(), deadline, p.stdout + p.stderr)
            time.sleep(0.1)
        # A valid outer relay cannot replace the independently pinned inner CA.
        self.assertEqual(json.loads(p.stdout)["status"], "tls_failure")
        wrong = self.run_cli("client", "net", "status", "--server-name", "wrong.invalid", "--ca", self.base / "coord/net/tls/ca.pem",
                             "--credential", self.base / "credential", *self.route(), success=False)
        self.assertEqual(wrong.returncode, 5, wrong.stdout)
        # Direct TCP succeeded, so a wrong certificate is a hard failure;
        # the client must not silently retry the operation via the good relay.
        direct = self.start("fake", "net", "serve", "--state-dir", self.base / "fake/net", "--port", "0")
        self.assertEqual(self.status("--dial", direct[1]["bind"]).returncode, 5)



@unittest.skipUnless(BRAIN, "set BRAIN_PROBE_BIN or BRAIN_BIN")
class RelayTeamDemo(unittest.TestCase):
    def test_two_machines_through_relay(self):
        p = subprocess.run([str(REPO / "tools/team-demo.sh"), "--via-relay", "--machines", "2"],
                           env=dict(os.environ, BRAIN_DEMO_BIN=str(Path(BRAIN).resolve())),
                           capture_output=True, text=True, timeout=115)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertIn("прямые адреса координаторов клиентам не передаются", p.stdout)
        self.assertIn("факт исчез", p.stdout)
        self.assertIn("нет ответа", p.stdout)
        self.assertIn("временные HOME удалены", p.stdout)


if __name__ == "__main__":
    unittest.main()
