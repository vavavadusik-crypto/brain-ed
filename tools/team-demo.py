#!/usr/bin/env python3
"""One-shot synthetic showcase; all state and credentials stay in temporary HOMEs."""
import argparse
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]


def binary():
    explicit = os.environ.get("BRAIN_DEMO_BIN") or os.environ.get("BRAIN_PROBE_BIN") or os.environ.get("BRAIN_BIN")
    if explicit:
        path = Path(explicit).resolve()
        if not path.is_file():
            raise RuntimeError("explicit demo binary does not exist")
        return str(path)
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
    for profile in ("debug", "release"):
        path = target / profile / "brain"
        if path.is_file():
            return str(path.resolve())
    raise RuntimeError("Build first: cargo build --locked -p brain-cli (or set BRAIN_DEMO_BIN)")


class Demo:
    def __init__(self, brain, base, via_relay=False):
        self.brain, self.base, self.servers = brain, base, []
        self.via_relay, self.routes = via_relay, {}

    def env(self, home):
        # Do not inherit live BRAIN_*, XDG_*, model/provider settings or Git config.
        return {"HOME": str(home), "BRAIN_HOME": str(home / ".brain"),
                "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "BRAIN_OFFLINE": "true",
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TEMPLATE_DIR": ""}

    def run(self, home, *args):
        if len(args) > 1 and args[0] == "net" and args[1] in ("init", "enroll"):
            args = (*args, "--state-dir", home / ".brain/net")
        elif args[:3] == ("net", "share", "grant"):
            args = (*args, "--state-dir", home / ".brain/net")
        if self.via_relay and args[:2] == ("net", "enroll") and "--bundle-out" in args:
            args = list(args)
            if "--dial" in args:
                i = args.index("--dial")
                del args[i:i + 2]
            args = (*args, *self.routes[home]["connect"])
        p = subprocess.run([self.brain, "--offline", *map(str, args)],
                           env=self.env(home), cwd=self.base, capture_output=True, text=True, timeout=15)
        if p.returncode:
            # Credential/bundle output is never printed, even on failure.
            raise RuntimeError(f"brain {args[0]} {args[1] if len(args) > 1 else ''} failed ({p.returncode})")
        return json.loads(p.stdout)

    def relay_setup(self):
        home = self.base / "relay"
        home.mkdir()
        self.relay_home = home
        self.run(home, "net", "init", "--name", "localhost")
        _, self.relay_dial = self.start_server(home, ["net", "relay", "serve", "--listen", "127.0.0.1",
                                                      "--port", "0", "--state-dir", home / ".brain/net"])
        print("Relay: сквозной TLS; прямые адреса координаторов клиентам не передаются", flush=True)

    def serve(self, home):
        route = []
        if self.via_relay:
            roles = {}
            for role in ("register", "connect"):
                token = self.base / f"{home.name}-{role}.relay-token"
                self.run(self.relay_home, "net", "relay", "token", "add", "--state-dir", self.relay_home / ".brain/net",
                         "--name", home.name, "--role", role, "--token-out", token)
                roles[role] = ["--via-relay", self.relay_dial, "--relay-name", home.name,
                               "--relay-token-file", token, "--relay-ca", self.relay_home / ".brain/net/tls/ca.pem",
                               "--relay-server-name", "localhost"]
            self.routes[home] = roles
            route = roles["register"]
        return self.start_server(home, ["net", "serve", "--state-dir", home / ".brain/net", "--port", "0", *route])

    def start_server(self, home, args):
        log = (home / "server.log").open("w")
        p = subprocess.Popen([self.brain, "--offline", *map(str, args)],
                             env=self.env(home), cwd=self.base, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=log, text=True)
        self.servers.append((p, log))
        with selectors.DefaultSelector() as selector:
            selector.register(p.stdout, selectors.EVENT_READ)
            if not selector.select(10):
                raise RuntimeError("loopback server startup timed out")
        ready = json.loads(p.stdout.readline())
        if ready.get("status") != "serving" or not ready["bind"].startswith("127.0.0.1:"):
            raise RuntimeError("server did not bind loopback")
        return p, ready["bind"]

    @staticmethod
    def stop(p):
        if p.poll() is None:
            p.stdin.close()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=3)

    def close(self):
        for p, log in reversed(self.servers):
            self.stop(p)
            p.stdout.close()
            if not p.stdin.closed:
                p.stdin.close()
            log.close()

    def seed(self, home, number):
        root = home / "project"
        root.mkdir()
        for args in (["git", "init", "-q", str(root)],
                     ["git", "-C", str(root), "-c", "user.name=Demo", "-c", "user.email=demo@example.invalid",
                      "commit", "--allow-empty", "-qm", "Synthetic checkout baseline"]):
            subprocess.run(args, env=self.env(home), cwd=self.base, capture_output=True, check=True, timeout=10)
        self.run(home, "init")
        now = int(time.time()) - 30
        with sqlite3.connect(home / ".brain/brain.sqlite") as c:
            c.execute("INSERT OR IGNORE INTO agents(id,created_at) VALUES('demo',?)", (now,))
            c.execute("INSERT OR IGNORE INTO spaces(id,kind,label,created_by_agent_id,created_at) VALUES('default','project','Synthetic demo','demo',?)", (now,))
            c.execute("INSERT INTO sessions(id,space_id,agent_id,started_at) VALUES('synthetic','default','demo',?)", (now,))
            facts = [
                ("error", "Checkout timeout — ошибка оформления заказа", "Retries exhausted; opened after load test", ["confirmed", "fixed", "reopened"]),
                ("error", "Resolved encoding — исправлена кодировка", "Added UTF-8 validation", ["confirmed", "fixed"]),
                ("decision", "Rejected global cache — решение не сработало", "Stale inventory after checkout; reverted", ["failed"]),
                ("plan", "Next sprint — план: backoff and metrics", "Reduce checkout latency before pilot", ["active"]),
                ("dead_end", "Abandoned polling — тупик: частый опрос", "Excess database load", ["abandoned"]),
            ]
            for index, (kind, what, why, states) in enumerate(facts):
                identity = f"demo-{index}"
                c.execute("INSERT INTO nodes(id,space_id,created_by_agent_id,created_in_session_id,created_at,last_accessed) VALUES(?,'default','demo','synthetic',?,?)", (identity, now, now))
                for revision, status in enumerate(states, 1):
                    at = now + revision
                    meta = {"distilled_fact": True, "kind": kind, "what": f"{what} (worker {number})", "why": why,
                            "status": status, "project": str(root), "evidence": [revision], "archive_session": "synthetic"}
                    end = at + 1 if revision < len(states) else None
                    c.execute("INSERT INTO node_versions(node_id,space_id,revision,kind,label,body,metadata,valid_from,valid_to,recorded_at,superseded_at,agent_id,session_id) VALUES(?,'default',?,?,?,?,?,?,?,?,?,'demo','synthetic')",
                              (identity, revision, "memory", "Synthetic fixture", "", json.dumps(meta, ensure_ascii=False), at, end, at, end))
        task = self.run(home, "work", "add", "checkout", f"Implement retry backoff — задача машины {number}")["task"]["id"]
        self.run(home, "work", "claim", task)
        self.run(home, "work", "add", "checkout", f"Review metrics — проверка метрик {number}", "--depends-on", task)
        return root

    def ask(self, boss, machines, title, query):
        print(f"\nРуководитель: {title}", flush=True)
        result = self.run(boss, "team", "ask", query, "--machines", ",".join(machines), "--limit", "40", "--deadline-ms", "5000")
        for line in result["lines"]:
            print("  " + line, flush=True)
        return result

    def showcase(self, count):
        if self.via_relay:
            self.relay_setup()
        boss = self.base / "leader"
        boss.mkdir()
        self.run(boss, "net", "init", "--name", "localhost")
        _, coord_dial = self.serve(boss)
        admin = boss
        if not self.via_relay:
            admin = self.base / "admin"
            admin.mkdir()
            self.run(admin, "net", "init", "--name", "localhost")
            _, coord_dial = self.serve(admin)
            invitation = self.base / "leader.invite.json"
            self.run(admin, "team", "invite", "--role", "manager", "--name", "leader",
                     "--project", "checkout", "--out", invitation)
            self.run(boss, "team", "accept", invitation)
        machines, homes, sources = [], [], []
        print("Brain ED — синтетическое демо команды (только 127.0.0.1; без моделей и внешних API)", flush=True)
        for n in range(1, count + 1):
            name = f"worker-{n}"
            home = self.base / name
            home.mkdir()
            machines.append(name)
            homes.append(home)
            bundle = self.base / f"{name}.bundle"
            if self.via_relay:
                self.run(boss, "net", "enroll", "--machine", name, "--agent-id", "demo", "--role", "worker",
                         "--grant-space", "demo", "--credential-out", self.base / f"{name}.credential",
                         "--bundle-out", bundle, "--dial", coord_dial)
                self.run(home, "net", "join", bundle, "--consume")
                for attempt in range(30):
                    try:
                        self.run(home, "net", "status")
                        break
                    except RuntimeError:
                        if attempt == 29:
                            raise
                        time.sleep(0.1)
                root = self.seed(home, n)
                self.run(home, "net", "init", "--name", "localhost")
                self.run(home, "team", "configure", "--machine", name, "--project", "checkout", "--root", root,
                         "--team-space", "team.checkout", "--recipient", "leader:lead")
                self.run(home, "team", "enable")
                self.run(home, "team", "publish")
                source, dial = self.serve(home)
                sources.append(source)
                reader = boss / f"{name}.reader"
                self.run(home, "net", "enroll", "--machine", "leader", "--agent-id", "lead", "--role", "reader",
                         "--grant-space", "team.checkout", "--credential-out", reader)
                self.run(home, "net", "share", "grant", "--machine", "leader", "--agent-id", "lead", "--space", "team.checkout")
                origin = self.run(home, "team", "manifest")["manifests"][0]["manifest"]["origin"]
                peer_route = self.routes[home]["connect"] if self.via_relay else ["--dial", dial]
                self.run(boss, "team", "peer", "--machine", name, "--origin", origin, "--project", "checkout",
                         "--team-space", "team.checkout", *peer_route, "--server-name", "localhost",
                         "--ca", home / ".brain/net/tls/ca.pem", "--credential", reader)
                print(f"Готова {name}: enroll → join → enable → publish; error lifecycle confirmed → fixed → reopened", flush=True)
            else:
                root = self.seed(home, n)
                self.run(home, "net", "init", "--name", "localhost")
                source, _ = self.serve(home)
                sources.append(source)
                self.run(admin, "team", "invite", "--role", "worker", "--name", name,
                         "--project", "checkout", "--out", bundle)
                self.run(home, "team", "accept", bundle, "--root", root, "--publish")
                bundle.unlink()  # Contains only a now-revoked one-time token.
                print(f"Готова {name}: invite → accept --publish; error lifecycle confirmed → fixed → reopened", flush=True)
        answers = [self.ask(boss, machines, title, query) for title, query in [
            ("Чем заняты машины?", "Чем заняты машины?"),
            ("Какие ошибки сейчас открыты?", "Какие ошибки сейчас открыты?"),
            ("Какие решения не сработали и почему?", "Какие решения не сработали и почему?"),
            ("Что в планах на следующий спринт?", "Что в планах на следующий спринт?")]]
        for answer, allowed in zip(answers, [{"task", "commit", "session", "participation"}, {"error"}, {"decision", "dead_end"}, {"plan"}]):
            if not answer["hits"] or any(h["fact"]["content"]["class"] not in allowed for h in answer["hits"]):
                raise RuntimeError("manager question selected the wrong fact classes")
            if {h["received_from_peer"] for h in answer["hits"]} != set(machines):
                raise RuntimeError("missing attributed synthetic answers")
        self.run(homes[0], "team", "disable", "--class", "error")
        immediate = self.ask(boss, [machines[0]], "Машина выключила класс error: старый срез закрыт", "reopened")
        if immediate["hits"] or immediate["machines"][0]["status"] != "denied":
            raise RuntimeError("class opt-out did not immediately deny stale slice")
        self.run(homes[0], "team", "publish")
        removed = self.ask(boss, [machines[0]], "Класс error выключен: факт исчез", "reopened")
        if removed["hits"] or removed["machines"][0]["status"] not in ("empty", "ok"):
            raise RuntimeError("disabled fact remained visible or source unavailable")
        print("Проверено: факт исчез после отключения класса error", flush=True)
        self.stop(sources[-1])
        offline = self.ask(boss, [machines[-1]], "Машина офлайн", "backoff")
        if offline["hits"] or offline["machines"][0]["status"] != "offline":
            raise RuntimeError("offline machine served cached facts")
        print("Проверено: офлайн — нет ответа; старые факты не выданы", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="store_true", help="retain synthetic HOMEs; servers still stop")
    parser.add_argument("--machines", type=int, choices=range(1, 17), default=3, metavar="N")
    parser.add_argument("--via-relay", action="store_true", help="run an isolated loopback relay; withhold direct coordinator addresses")
    args = parser.parse_args()
    brain = binary()
    base = Path(tempfile.mkdtemp(prefix="brain-team-demo-"))
    demo = Demo(brain, base, args.via_relay)
    started = time.monotonic()
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    try:
        demo.showcase(args.machines)
    finally:
        demo.close()
        if args.keep:
            print(f"Сохранено (--keep): {base}; серверы остановлены", flush=True)
        else:
            shutil.rmtree(base)
            print("Уборка: временные HOME удалены; серверы остановлены", flush=True)
    print(f"Демо завершено за {time.monotonic() - started:.1f} с", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        print(f"Demo failed: {error}", file=sys.stderr)
        sys.exit(1)
