"""Tests for tools/agent_bridge.py.

Two layers, kept distinct on purpose:

* AgentBridgeSchemaTest / AgentBridgeTransportTest -- mocked-subprocess unit
  tests. No Brain binary, no database; they prove the schema and the
  transport failure mapping.
* RealBinaryBridgeTest -- exercises the actual installed `brain` binary in a
  synthetic HOME (temp dir). This is still not a real model-client
  integration; no provider is contacted and no live configuration is read.
"""
import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "tools" / "agent_bridge.py"
_spec = importlib.util.spec_from_file_location("agent_bridge", MODULE_PATH)
ab = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ab)


def run_bridge(request, stream_text=None, env=None):
    text = stream_text if stream_text is not None else json.dumps(request)
    out, err = io.StringIO(), io.StringIO()
    old_env = os.environ.copy()
    os.environ.update(env or {})
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = ab.main(stream=io.StringIO(text))
    finally:
        os.environ.clear()
        os.environ.update(old_env)
    return code, json.loads(out.getvalue()), out.getvalue() + err.getvalue()


BASE = {"v": 1, "client": "codex", "space": "s", "session": "sess-1"}


class AgentBridgeSchemaTest(unittest.TestCase):
    # A deliberately nonexistent binary: a schema-VALID request surfaces as
    # config_error and never contacts the live default coordinator. Schema-INVALID
    # requests are refused before any binary lookup, so this keeps every schema
    # test hermetic regardless of what is installed.
    DEAD = {ab.BINARY_ENV: "/nonexistent/brain-xyz-for-schema-tests"}

    def request(self, **fields):
        request = dict(BASE, op="context")
        request.update(fields)
        return request

    def test_unknown_field_refused(self):
        code, doc, _ = run_bridge(self.request(binary="/bin/echo", op="context"))
        self.assertEqual(doc["status"], "invalid")
        self.assertEqual(doc["outcome_kind"], "invalid_input")
        self.assertIn("unknown request fields", doc["result"]["hint"])
        self.assertEqual(code, 1)

    def test_version_must_be_integer_one(self):
        for bad in (2, "1", 1.0, True):
            code, doc, _ = run_bridge(self.request(v=bad))
            self.assertEqual(doc["status"], "invalid", bad)

    def test_client_and_op_are_enumerated(self):
        for bad_client in ("claude3", "CURSOR", "", 3):
            code, doc, _ = run_bridge(self.request(client=bad_client))
            self.assertEqual(doc["status"], "invalid", bad_client)
        for bad_op in ("begin", "status", "", "CONTEXT"):
            code, doc, _ = run_bridge(self.request(op=bad_op))
            self.assertEqual(doc["status"], "invalid", bad_op)

    def test_integer_limits_reject_floats_strings_and_bools(self):
        for field, value in [("task_id", 1.5), ("task_id", "7"), ("task_id", True),
                             ("fence", -3), ("claim_id", 0), ("budget_bytes", 10**9),
                             ("ttl_secs", 86401)]:
            code, doc, _ = run_bridge(self.request(op="claim", project="p",
                                                   **{field: value}))
            self.assertEqual(doc["status"], "invalid", (field, value))
            self.assertEqual(doc["outcome_kind"], "invalid_input")

    def test_boolean_board_filter_ready_only(self):
        code, doc, _ = run_bridge(self.request(board_filter={"ready": "yes"}))
        self.assertEqual(doc["status"], "invalid")
        code, doc, _ = run_bridge(self.request(board_filter={"limit": 0}))
        self.assertEqual(doc["status"], "invalid")
        code, doc, _ = run_bridge(self.request(board_filter={"wat": 1}))
        self.assertEqual(doc["status"], "invalid")

    def test_required_fields_per_operation(self):
        cases = [
            ({"op": "claim"}, "task_id", "invalid"),
            ({"op": "claim", "task_id": 1}, "project", "invalid"),
            ({"op": "renew", "claim_id": 1}, "fence", "invalid"),
            # release with claim+fence is schema-valid: it passes the schema
            # gate and proceeds to the (dead) binary -> config_error, never a
            # local "invalid".
            ({"op": "release", "claim_id": 1, "fence": 2}, None, "config_error"),
            ({"op": "finish", "claim_id": 1, "fence": 2}, "outcome", "invalid"),
            ({"op": "finish", "claim_id": 1, "fence": 2, "outcome": "paused"},
             "outcome", "invalid"),
        ]
        for fields, missing, expected in cases:
            code, doc, _ = run_bridge(self.request(**fields), env=self.DEAD)
            self.assertEqual(doc["status"], expected, fields)
            if missing:
                self.assertIn(missing, doc["result"]["hint"])

    def test_space_and_token_charset(self):
        for bad in ("has space", "uni☃code", "x" * 65, ""):
            code, doc, _ = run_bridge(self.request(space=bad))
            self.assertEqual(doc["status"], "invalid", bad)

    def test_request_id_printable_ascii_and_bounded(self):
        for bad in ("has\ttab", "snowman☃", "x" * 129):
            code, doc, _ = run_bridge(self.request(op="claim", task_id=1, project="p",
                                                   request_id=bad))
            self.assertEqual(doc["status"], "invalid", bad)

    def test_project_segments_and_charset(self):
        for bad in ("../escape", "a//b", "a/./b", "/abs", "trail/", "a b", "x" * 101):
            code, doc, _ = run_bridge(self.request(op="claim", task_id=1, project=bad))
            self.assertEqual(doc["status"], "invalid", bad)
        code, doc, _ = run_bridge(self.request(op="claim", task_id=1,
                                               project="src/lib.rs"), env=self.DEAD)
        # Path-looking projects are fine for the coordinator; only schema
        # violations are refused locally. This one passes schema and proceeds to
        # the (dead) binary -> config_error, never a schema "invalid".
        self.assertEqual(doc["status"], "config_error")
        self.assertNotEqual(doc["outcome_kind"], "invalid_input")

    def test_oversized_and_malformed_stdin(self):
        code, doc, _ = run_bridge({}, stream_text="{")
        self.assertEqual(doc["status"], "invalid")
        code, doc, _ = run_bridge({}, stream_text="[" + "1]")
        self.assertEqual(doc["status"], "invalid")
        code, doc, _ = run_bridge({}, stream_text="x" * (ab.MAX_REQUEST_BYTES + 2))
        self.assertEqual(doc["status"], "invalid")

    def test_agent_identity_is_derived_and_bounded(self):
        a1 = ab.derive_agent("codex", "sess-1")
        a2 = ab.derive_agent("codex", "sess-2")
        a3 = ab.derive_agent("claude", "sess-1")
        self.assertRegex(a1, r"^codex-[0-9a-f]{12}$")
        self.assertNotEqual(a1, a2)
        self.assertNotEqual(a1, a3)
        self.assertTrue(all(ch.isascii() and (ch.isalnum() or ch == "-") for ch in a1))

    def test_lone_surrogate_rejected_invalid_not_traceback(self):
        # Regression #5: JSON-escaped lone surrogates must be a stable
        # invalid, never a crash, for every string field.
        for field in ("note", "project", "session", "space"):
            raw = '{"v":1,"client":"codex","space":"s","session":"ok","op":"context",' \
                  '"board_filter":{},"%s":"\\ud800"}' % field
            code, doc, _ = run_bridge({}, stream_text=raw, env=self.DEAD)
            self.assertEqual(doc["status"], "invalid", field)
            self.assertEqual(doc["outcome_kind"], "invalid_input")
        # Nested inside board_filter too.
        raw = ('{"v":1,"client":"codex","space":"s","session":"ok","op":"context",'
               '"board_filter":{"state":"\\udfff"}}')
        code, doc, _ = run_bridge({}, stream_text=raw, env=self.DEAD)
        self.assertEqual(doc["status"], "invalid")

    def test_note_is_bounded_in_utf8_bytes(self):
        # Regression #5: 1000 Cyrillic chars = 2000 bytes is OK; 1001 is over.
        # Run against a nonexistent binary so a schema-valid note returns
        # config_error (never reaching the live default coordinator).
        dead = {ab.BINARY_ENV: "/nonexistent/brain-xyz"}
        code, doc, _ = run_bridge(dict(BASE, op="release", claim_id=1, fence=2,
                                       note="ё" * 1000), env=dead)
        self.assertEqual(doc["status"], "config_error")
        self.assertNotEqual(doc["outcome_kind"], "invalid_input")
        code, doc, _ = run_bridge(dict(BASE, op="release", claim_id=1, fence=2,
                                       note="ё" * 1001), env=dead)
        self.assertEqual(doc["status"], "invalid")
        self.assertIn("bytes", doc["result"]["hint"])

    def test_project_is_ascii_only(self):
        # Regression #5: str.isalnum() accepts Unicode; project must be ASCII.
        for badproj in ("projё", "projeté", "проект"):
            code, doc, _ = run_bridge(dict(BASE, op="claim", task_id=1,
                                           project=badproj))
            self.assertEqual(doc["status"], "invalid", badproj)


class AgentBridgeTransportTest(unittest.TestCase):
    """Real child processes via a shell-script fake coordinator.

    The bridge is proven never to run attacker bytes, so using `sh` as the
    fake `brain` here is a legitimate test double, exercised through the real
    bounded-capture transport (Popen + non-blocking read), not a patched
    subprocess.run.
    """

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="bridge-fake-")
        self.fake = Path(self.tmp, "fake-brain")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def write_fake(self, body):
        self.fake.write_text(body, encoding="utf-8")
        self.fake.chmod(0o755)
        return str(self.fake)

    def request(self, **fields):
        request = dict(BASE, op="claim", task_id=7, project="p")
        request.update(fields)
        return request

    def run_with_fake(self, request, script):
        binary = self.write_fake(script)
        env = {ab.BINARY_ENV: binary, "BRAIN_OFFLINE": "true"}
        return run_bridge(request, env=env)

    def test_hostile_strings_are_literal_argv(self):
        note = '"; touch /tmp/pwned_xyz; $(touch /tmp/pwned_xyz); `touch /tmp/pwned_xyz`\nsnowman'
        script = (
            '#!/bin/sh\n'
            '# claim path: echo a busy refusal so we can inspect argv\n'
            'printf \'%s\' \'{"status":"error","error_kind":"busy","hint":"taken"}\'\n'
        )
        code, doc, _ = self.run_with_fake(self.request(op="release", claim_id=3,
                                                       fence=9, note=note), script)
        self.assertEqual(doc["status"], "refused")
        self.assertEqual(doc["outcome_kind"], "busy")
        # The shell-metachar note stayed a literal argv element; nothing ran.
        self.assertFalse(os.path.exists("/tmp/pwned_xyz"))

    def test_no_shell_argument_array_shape(self):
        # claim re-reads the task (work show) for the project guard, then
        # claims. The fake answers the show with the matching project and the
        # claim with ok, proving scalars arrive as separate argv elements
        # (the script would break if joined through a shell).
        script = (
            '#!/bin/sh\n'
            'for a in "$@"; do\n'
            '  if [ "$a" = "show" ]; then\n'
            "    printf '%s' '{\"status\":\"ok\",\"task\":{\"id\":7,\"project\":\"p\"}}'\n"
            '    exit 0\n'
            '  fi\n'
            'done\n'
            "printf '%s' '{\"status\":\"ok\",\"claim\":{\"fence\":2}}'\n"
        )
        code, doc, _ = self.run_with_fake(self.request(), script)
        self.assertEqual(doc["status"], "ok", doc)
        self.assertEqual(code, 0)

    def test_timeout_is_transport_unknown_and_child_reaped(self):
        script = '#!/bin/sh\nsleep 60\n'  # hangs; must be killed, not left
        import time as _t
        t0 = _t.monotonic()
        code, doc, _ = self.run_with_fake(self.request(request_id="r-9"),
                                          script)
        elapsed = _t.monotonic() - t0
        self.assertEqual(doc["status"], "transport_unknown")
        self.assertEqual(doc["outcome_kind"], "unavailable")
        self.assertEqual(code, 1)
        self.assertNotIn("claim", doc["result"])
        self.assertLess(elapsed, 40, "child must be killed at the deadline")
        # renew/release must not promise blind retry in the hint.
        code, doc2, _ = self.run_with_fake(
            self.request(op="renew", claim_id=1, fence=2), script)
        self.assertEqual(doc2["status"], "transport_unknown")
        self.assertIn("verify", doc2["result"]["hint"])

    def test_flooding_child_is_capped_and_killed(self):
        flood = "#!/bin/sh\nwhile true; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; done\n"
        code, doc, _ = self.run_with_fake(self.request(), flood)
        # 1 MiB stdout cap exceeded -> transport_unknown, no partial accepted.
        self.assertEqual(doc["status"], "transport_unknown")
        self.assertEqual(doc["outcome_kind"], "unavailable")
        self.assertIn("cap", json.dumps(doc["result"]))

    def test_child_closing_pipes_then_hanging_still_times_out(self):
        # Regression: both pipes reach EOF but the child then never exits.
        # proc.wait() must respect the remaining deadline, then kill+reap.
        import time as _t
        # Closes stdout and stderr immediately, then hangs forever.
        script = '#!/bin/sh\nexec 1>&- 2>&-\nexec 0</dev/null\nsleep 60\n'
        t0 = _t.monotonic()
        code, doc, _ = self.run_with_fake(self.request(), script)
        elapsed = _t.monotonic() - t0
        self.assertEqual(doc["status"], "transport_unknown")
        self.assertEqual(doc["outcome_kind"], "unavailable")
        self.assertIn("did not exit", doc["result"]["hint"])
        self.assertLess(elapsed, 40, "closed-pipe hang must be killed at the deadline")

    def test_mangled_output_is_transport_unknown(self):
        script = '#!/bin/sh\nprintf \'%s\' \'not json at all\'\necho panic 1>&2\nexit 2\n'
        code, doc, _ = self.run_with_fake(self.request(), script)
        self.assertEqual(doc["status"], "transport_unknown")

    def test_missing_binary_is_config_error(self):
        env = {ab.BINARY_ENV: "/nonexistent/brain-xyz"}
        code, doc, _ = run_bridge(self.request(), env=env)
        self.assertEqual(doc["status"], "config_error")
        self.assertEqual(doc["outcome_kind"], "not_configured")

    def test_network_and_offline_flags_honest(self):
        script = '#!/bin/sh\nprintf \'%s\' \'{"status":"ok"}\'\n'
        code, doc, _ = self.run_with_fake(self.request(), script)
        self.assertTrue(doc["offline"])
        self.assertFalse(doc["network_capable"])

    def test_renew_release_refuse_request_id(self):
        # Regression #3: no implied retry safety for non-idempotent ops.
        for op, fields in [("renew", {"claim_id": 1, "fence": 2}),
                           ("release", {"claim_id": 1, "fence": 2})]:
            code, doc, _ = run_bridge(dict(BASE, op=op, request_id="x-1", **fields))
            self.assertEqual(doc["status"], "invalid", op)
            self.assertIn("request_id", doc["result"]["hint"])

    def test_note_and_ttl_confined_to_their_ops(self):
        code, doc, _ = run_bridge(dict(BASE, op="claim", task_id=1, project="p",
                                       note="x"))
        self.assertEqual(doc["status"], "invalid")
        code, doc, _ = run_bridge(dict(BASE, op="finish", claim_id=1, fence=2,
                                       outcome="completed", ttl_secs=120))
        self.assertEqual(doc["status"], "invalid")


@unittest.skipUnless(shutil.which("brain"), "installed brain binary required")
class RealBinaryBridgeTest(unittest.TestCase):
    """Real installed binary, synthetic HOME. Not a provider integration."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="bridge-test-")
        Path(self.tmp, ".brain").mkdir()
        self.binary = shutil.which("brain")
        self.env = {"HOME": self.tmp, "BRAIN_OFFLINE": "true"}
        for key in ("BRAIN_DATABASE", "BRAIN_AGENT", "BRAIN_MACHINE_ID"):
            self.env[key] = ""
        os.environ.pop("BRAIN_DATABASE", None)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def brain(self, *args, agent="seed"):
        env = dict(os.environ, HOME=self.tmp, BRAIN_OFFLINE="true")
        env.pop("BRAIN_DATABASE", None)
        env.pop("BRAIN_AGENT", None)
        out = subprocess.run(
            [self.binary, "--space", "s", "--agent", agent, "work", *args],
            capture_output=True, env=env, timeout=30, check=False)
        return out.returncode, json.loads(out.stdout.decode())

    def bridge(self, request, **env_extra):
        text = json.dumps(request)
        env = dict(os.environ, HOME=self.tmp, BRAIN_OFFLINE="true", **env_extra)
        env.pop("BRAIN_DATABASE", None)
        env.pop("BRAIN_AGENT", None)
        out = subprocess.run(
            ["python3", str(MODULE_PATH)], input=text.encode(),
            capture_output=True, env=env, timeout=60, check=False)
        return out.returncode, json.loads(out.stdout.decode())

    def seed_task(self, title="bridge test task"):
        code, doc = self.brain("add", "proj", title, "--request-id", "seed-1")
        self.assertEqual(code, 0, doc)
        return doc["task"]["id"]

    def test_three_client_identity_traces_and_cross_session_renew(self):
        task = self.seed_task()
        claims, refused_others = {}, {}
        for client in ("codex", "claude", "hermes"):
            code, doc = self.bridge(dict(BASE, client=client, session=f"{client}-s1",
                                         op="claim", task_id=task, project="proj",
                                         request_id=f"c-{client}-1"))
            if doc["status"] == "ok":
                claims[client] = doc
            else:
                refused_others[client] = doc
        self.assertEqual(set(claims), {"codex"})
        self.assertEqual(set(refused_others), {"claude", "hermes"})
        for other in refused_others.values():
            self.assertEqual(other["status"], "refused")
            self.assertEqual(other["outcome_kind"], "busy")
        mine = claims["codex"]
        self.assertTrue(mine["agent"].startswith("codex-"), mine["agent"])
        mine_claim = mine["result"]["claim"]

        # The same logical request replayed returns the stored answer.
        code2, replay = self.bridge(dict(BASE, client="codex", session="codex-s1",
                                         op="claim", task_id=task, project="proj",
                                         request_id="c-codex-1"))
        self.assertTrue(replay["result"].get("replayed"), replay)
        self.assertEqual(replay["result"]["claim"]["claim_id"], mine_claim["claim_id"])

        # A different session of the same client is a different agent and
        # cannot renew or finish the first session's claim.
        other = self.bridge(dict(BASE, client="codex", session="codex-s2",
                                 op="renew", claim_id=mine_claim["claim_id"],
                                 fence=mine_claim["fence"], ttl_secs=120))[1]
        self.assertEqual(other["status"], "refused")
        self.assertEqual(other["outcome_kind"], "not_owner")

        # A stale fence after a successful self-renew stays refused, not fake.
        renewed = self.bridge(dict(BASE, client="codex", session="codex-s1",
                                   op="renew", claim_id=mine_claim["claim_id"],
                                   fence=mine_claim["fence"], ttl_secs=300))[1]
        self.assertEqual(renewed["status"], "ok", renewed)
        stale = self.bridge(dict(BASE, client="codex", session="codex-s1",
                                 op="renew", claim_id=mine_claim["claim_id"],
                                 fence=mine_claim["fence"] + 99, ttl_secs=300))[1]
        self.assertEqual(stale["status"], "refused")
        self.assertEqual(stale["outcome_kind"], "stale_fence")

        finished = self.bridge(dict(BASE, client="codex", session="codex-s1",
                                    op="finish", claim_id=mine_claim["claim_id"],
                                    fence=mine_claim["fence"], outcome="completed",
                                    note="done", request_id="f-codex-1"))[1]
        self.assertEqual(finished["status"], "ok", finished)
        self.assertEqual(finished["result"]["task"]["state"], "completed")

        released = self.bridge(dict(BASE, client="codex", session="codex-s1",
                                    op="release", claim_id=mine_claim["claim_id"],
                                    fence=mine_claim["fence"]))[1]
        self.assertEqual(released["status"], "refused")
        self.assertEqual(released["outcome_kind"], "invalid_state")

    def test_context_no_archive_canary_and_budget(self):
        canary = "PRIVATE_ARCHIVE_CANARY_ZQ7"
        task = self.seed_task()
        code, packet = self.brain("show", str(task))
        # Poison the memory graph and archive-free coordinator with a marker,
        # then confirm context never includes archive content by construction.
        env = dict(os.environ, HOME=self.tmp, BRAIN_OFFLINE="true")
        subprocess.run([self.binary, "--space", "s", "--agent", "seed",
                        "remember", canary + " must never appear"],
                       capture_output=True, env=env, timeout=30, check=False)
        code, doc = self.bridge(dict(BASE, client="hermes", session="h-1",
                                     op="context", task_id=task,
                                     budget_bytes=4096))
        self.assertEqual(doc["status"], "ok", doc)
        serialized = json.dumps(doc, ensure_ascii=False).encode()
        self.assertLessEqual(len(serialized), 4096 + 4096)  # envelope around it bounded too
        self.assertNotIn(canary, json.dumps(doc, ensure_ascii=False))
        result = doc["result"]
        self.assertIn("references", result)
        self.assertIn("memory", result["references"])
        self.assertTrue(result["data_is_agent_supplied"])
        self.assertIn("agent_supplied", result.get("trust", ""))
        # The selected task packet is present with its trust label intact.
        self.assertEqual(result["task"]["task"]["id"], task)
        self.assertIn("agent_supplied", result["task"]["trust"])

    def test_context_budget_truncation_unicode_safe(self):
        body = "ё" * 90  # multi-byte; "unicode " + 90*2 = 188 bytes, under the cap
        code, added = self.brain("add", "proj", "unicode " + body,
                                 "--request-id", "seed-u")
        self.assertEqual(code, 0, added)
        task = added["task"]["id"]
        code, doc = self.bridge(dict(BASE, client="claude", session="c-9",
                                     op="context", task_id=task,
                                     budget_bytes=2048))
        self.assertEqual(doc["status"], "ok")
        raw = json.dumps(doc["result"], ensure_ascii=False).encode()
        self.assertLessEqual(len(raw), 2048 + 400)  # small envelope slack
        # Valid JSON with valid unicode even after cutting.
        json.loads(raw.decode("utf-8"))
        if doc["truncated"]:
            self.assertTrue(doc["result"].get("omitted_board_tasks") is not None
                            or doc["result"].get("task", {}).get("truncated_fields")
                            or doc["result"].get("omitted_events"),
                            doc["result"])

    def test_whoami_identity_is_installation_not_request(self):
        _, who = self.brain("whoami")
        self.assertTrue(who["machine"].startswith("m-"), who)
        self.assertIn("installation id", who.get("machine_source", ""), who)
        # The bridge reads the same coordinator the CLI uses, under the home
        # the host configured; the request cannot name a machine or database.
        _, doc = self.bridge(dict(BASE, client="hermes", session="s-x", op="context"))
        self.assertIn(doc["status"], ("ok", "refused"), doc)
        if doc["status"] == "ok":
            self.assertEqual(doc["result"]["board"]["space"], "s")
        else:
            # No coordinator database yet: the honest not_configured, never a
            # fabricated board.
            self.assertEqual(doc["outcome_kind"], "not_configured", doc)
        self.assertFalse(doc["network_capable"])

    def test_whole_document_budget_counts_envelope_and_newline(self):
        # Regression #1: budget bounds the WHOLE emitted UTF-8 doc with the
        # exact emit encoding, escaping, markers and newline included. Seed
        # enough tasks that the board alone overflows a 1024 budget, forcing
        # the trim of whole board tasks before any text is cut.
        last_id = None
        for n in range(6):
            code, added = self.brain("add", "proj", "pad task number %d éé" % n,
                                     "--request-id", "seed-big-%d" % n)
            self.assertEqual(code, 0, added)
            last_id = added["task"]["id"]
        code, doc = self.bridge(dict(BASE, client="codex", session="b-1",
                                     op="context", task_id=last_id,
                                     budget_bytes=1024))
        actual = len((json.dumps(doc, ensure_ascii=False, sort_keys=True) + "\n")
                     .encode("utf-8"))
        self.assertLessEqual(actual, 1024, f"emitted doc {actual} > 1024")
        self.assertTrue(doc["truncated"])
        # Either a trimmed ok or a minimal budget_exceeded refusal; never a
        # claim that the budget was met while overflowing.
        self.assertIn(doc["status"], ("ok", "refused"))

    def test_claim_refuses_project_mismatch(self):
        # Regression #4: claim must not grab a task whose real project differs.
        code, a = self.brain("add", "alpha", "task in alpha", "--request-id", "pa")
        ta = a["task"]["id"]
        code, b = self.brain("add", "beta", "task in beta", "--request-id", "pb")
        tb = b["task"]["id"]
        # Ask for alpha's task while naming project beta -> refused.
        _, doc = self.bridge(dict(BASE, client="codex", session="pm-1",
                                  op="claim", task_id=ta, project="beta"))
        self.assertEqual(doc["status"], "refused")
        self.assertEqual(doc["outcome_kind"], "project_mismatch")
        # Task stays claimable by the correct project.
        _, okdoc = self.bridge(dict(BASE, client="codex", session="pm-1",
                                    op="claim", task_id=ta, project="alpha",
                                    request_id="c-pm-1"))
        self.assertEqual(okdoc["status"], "ok", okdoc)


if __name__ == "__main__":
    unittest.main()
