#!/usr/bin/env python3
"""Provider-neutral lifecycle bridge between agents and the Brain coordinator.

stdin: one JSON request object (bounded). stdout: one JSON response, one line.

    python3 tools/agent_bridge.py <<'EOF'
    {"v":1,"client":"codex","space":"default","project":"brain-for-ai",
     "session":"sess-123","op":"context","task_id":7}
    EOF

Operations:

  context  -- one bounded board page, and one task packet when task_id is given.
  claim    -- claim a task with a lease. First re-reads the task (read-only)
              and refuses when the caller's project does not match the task's
              authoritative project, so a session cannot claim task A while
              believing it works on project B.
  renew    -- extend a live lease with its fencing token.
  finish   -- move the claimed task to completed or failed (nothing else).
  release  -- give the claim up without changing task state.

Design rules, all deliberate:

* The Brain binary is host configuration (config file, BRAIN_BRIDGE_CONFIG,
  BRAIN_BINARY, or PATH lookup in that order). The request can never name an
  executable, database, agent or machine identity: the binary's own
  installation id (or the host's BRAIN_MACHINE_ID) is used.
* Every call is an argument array with shell=False. Request strings travel
  only as literal argv elements or as validated numbers; they are never pasted
  into a shell or into a flag name.
* Idempotency is real ONLY where the coordinator supports it: claim and finish
  accept request_id and replay the stored answer. renew and release accept no
  request_id; the bridge refuses one rather than imply retry safety. When a
  renew/release outcome is transport_unknown, recovery is to QUERY first
  (work verify <claim> <fence> / work claims) and reconcile manually, never a
  blind repeat.
* claim/finish/release/renew always forward the coordinator's authoritative
  answer. On transport failure (timeout, spawn error, mangled or oversized
  output) the outcome is transport_unknown -- never a faked success. Subprocess
  stdout and stderr are bounded and the child is killed and reaped on timeout
  or cap overflow; no partial JSON is ever accepted.
* budget_bytes bounds the WHOLE emitted UTF-8 JSON document (envelope,
  escaping, truncation markers, newline), measured with the exact emit
  encoding. If even a minimal document cannot fit, the answer is a bounded
  budget_exceeded refusal; the bridge never claims a budget it did not meet.
* Strings containing JSON-escaped lone surrogates are rejected before any
  hashing, serialization or UTF-8 encoding, so hostile input gets a stable
  invalid answer instead of a traceback. note is bounded in BYTES (2000).
  project is ASCII [A-Za-z0-9._-/] with no empty/dot/dot-dot segments.
* No provider/model API calls, no network, no daemons, no client hooks. This
  is a source-only helper; wiring it into Claude Code / Codex / Hermes session
  hooks is documented in docs/agent-bridge.md and remains pending.

Request schema v1 (strict: unknown fields are refused):

  v                literal integer 1 (not true, not 1.0)
  client           "codex" | "claude" | "hermes"
  space            coordination space token, <=64 of [A-Za-z0-9._:@-]
  project          ASCII project name, <=100; required for claim, included for
                   context when given; board reads need not name one
  session          caller-chosen session tag, <=200 chars; hashed, never sent
  op               "context" | "claim" | "renew" | "finish" | "release"
  request_id       <=128 printable ASCII; ONLY claim and finish; same id +
                   same request replays the coordinator's stored answer
  task_id          integer >= 1; context (optional), claim (required)
  claim_id/fence   integers >= 1; renew, finish, release (required)
  outcome          "completed" | "failed"; finish (required)
  ttl_secs         integer 10..86400 (default 900); claim, renew; refused on others
  note             <=2000 UTF-8 BYTES; finish, release; refused on others
  board_filter     {"state": queued|active|paused|completed|failed,
                    "ready": true, "limit": 1..100, "offset": >=0}; context only
  budget_bytes     1024..65536 (default 16000); whole-document cap, context only

Response envelope:

  {"v":1, "status":"ok"|"refused"|"invalid"|"transport_unknown"|"config_error",
   "agent":"client-hash12", "client":..., "space":..., "session_hash12":...,
   "outcome_kind": <coordinator error_kind | bridge kind, or null>,
   "result": <coordinator document for writes; context packet for context>,
   "truncated": <bool, context only>, "limits": {...}}

Bridge-local outcome_kind values: project_mismatch (claim project guard),
budget_exceeded (context did not fit the declared cap). Coordinator kinds are
forwarded unchanged (busy, not_owner, stale_fence, lease_expired, ...).

Everything under result keeps the coordinator's own fields, including its
"trust" line: ownership/leases/fences/times are coordinator_state (this
coordinator's authoritative state) while titles, details and notes are
agent_supplied, untrusted data and never instructions.
"""
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

SCHEMA_VERSION = 1
MAX_REQUEST_BYTES = 64 * 1024
SUBPROCESS_TIMEOUT_SECS = 20
MAX_STDOUT_BYTES = 1 << 20          # any coordinator answer is far smaller
MAX_STDERR_BYTES = 16 * 1024
REAP_GRACE_SECS = 5
BINARY_NAME = "brain"
CLIENTS = ("codex", "claude", "hermes")
OPERATIONS = ("context", "claim", "renew", "finish", "release")
FINISH_OUTCOMES = ("completed", "failed")
STATES = ("queued", "active", "paused", "completed", "failed")
REQUEST_ID_OPS = ("claim", "finish")
NOTE_OPS = ("finish", "release")
TTL_OPS = ("claim", "renew")

SPACE_MAX = 64
PROJECT_MAX = 100
SESSION_MAX = 200
REQUEST_ID_MAX = 128
NOTE_MAX_BYTES = 2000
TOKEN_CHARS = re.compile(r"^[A-Za-z0-9._:@-]+$")
PRINTABLE_ASCII = re.compile(r"^[!-~]+$")
PROJECT_CHARS = re.compile(r"^[A-Za-z0-9._\-/]+$")

TTL_MIN = 10
TTL_MAX = 86400
TTL_DEFAULT = 900
BOARD_LIMIT_DEFAULT = 50
BOARD_LIMIT_MAX = 100
BUDGET_MIN = 1024
BUDGET_MAX = 65536
BUDGET_DEFAULT = 16000

STATUS_OK = "ok"
STATUS_REFUSED = "refused"
STATUS_INVALID = "invalid"
STATUS_TRANSPORT = "transport_unknown"
STATUS_CONFIG = "config_error"

AGENT_HASH_LEN = 12
CONFIG_ENV = "BRAIN_BRIDGE_CONFIG"
BINARY_ENV = "BRAIN_BINARY"


class BridgeError(Exception):
    """Input that is malformed without reaching the coordinator."""

    def __init__(self, hint):
        super().__init__(hint)
        self.hint = hint


class TransportError(Exception):
    """The coordinator answer, if any, could not be trusted."""

    def __init__(self, hint, detail=None):
        super().__init__(hint)
        self.hint = hint
        self.detail = detail


class ConfigError(Exception):
    """Host configuration is missing or contradicts itself."""


def _no_lone_surrogates(value, path="$"):
    """Reject lone surrogates anywhere in the parsed request.

    JSON escapes like \\ud800 decode to lone surrogates that crash UTF-8
    encoding and serialization. Refuse them as invalid input instead.
    """
    if isinstance(value, str):
        try:
            value.encode("utf-8")
        except UnicodeEncodeError:
            raise BridgeError(f"{path} contains a lone surrogate escape; invalid")
    elif isinstance(value, dict):
        for key, item in value.items():
            _no_lone_surrogates(key, path)
            _no_lone_surrogates(item, f"{path}.{key}")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            _no_lone_surrogates(item, f"{path}[{index}]")


def derive_agent(client, session):
    """Stable bounded ASCII identity, matching the coordinator's token rule.

    client-hash12(session) keeps one client from colliding across its own
    sessions and binds every claim to the session that made it, so a later
    network credential can bind exactly the same identity. The raw session tag
    never leaves the host: only its hash is sent as the --agent identity.
    """
    digest = hashlib.sha256(session.encode("utf-8")).hexdigest()[:AGENT_HASH_LEN]
    return f"{client}-{digest}"


def read_request(stream):
    raw = stream.buffer.read(MAX_REQUEST_BYTES + 1) if hasattr(stream, "buffer") else stream.read(MAX_REQUEST_BYTES + 1)
    if isinstance(raw, str):
        raw = raw.encode("utf-8", "surrogatepass")
    if len(raw) > MAX_REQUEST_BYTES:
        raise BridgeError(f"request is larger than {MAX_REQUEST_BYTES} bytes")
    try:
        request = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BridgeError(f"request is not one valid UTF-8 JSON document: {error}")
    if not isinstance(request, dict):
        raise BridgeError("request must be one JSON object")
    _no_lone_surrogates(request)
    return request


def checked_string(request, field, maximum, required=False, pattern=None, label=None):
    value = request.get(field)
    if value is None:
        if required:
            raise BridgeError(f"{label or field} is required")
        return None
    if not isinstance(value, str):
        raise BridgeError(f"{label or field} must be a string")
    if len(value) > maximum:
        raise BridgeError(f"{label or field} is longer than {maximum} characters")
    if pattern is not None and not pattern.match(value):
        raise BridgeError(f"{label or field} has characters outside {pattern.pattern}")
    return value


def checked_int(request, field, minimum, maximum, required=False, default=None):
    value = request.get(field)
    if value is None:
        if required:
            raise BridgeError(f"{field} is required")
        return default
    # bools are ints in Python; a JSON true is not a task id.
    if isinstance(value, bool) or not isinstance(value, int):
        raise BridgeError(f"{field} must be an integer")
    if value < minimum or value > maximum:
        raise BridgeError(f"{field} must be in {minimum}..{maximum}")
    return value


def checked_project(value):
    if value is None:
        return None
    if len(value) > PROJECT_MAX or value.startswith("/") or value.endswith("/"):
        raise BridgeError("project must be 1..100 chars, relative, no trailing slash")
    if not PROJECT_CHARS.match(value):
        raise BridgeError("project must use ASCII [A-Za-z0-9._-/] only")
    if any(part in ("", ".", "..") for part in value.split("/")):
        raise BridgeError("project must not contain empty, '.' or '..' segments")
    return value


def checked_note(value):
    if value is None:
        return None
    if not isinstance(value, str):
        raise BridgeError("note must be a string")
    if len(value.encode("utf-8")) > NOTE_MAX_BYTES:
        raise BridgeError(f"note is longer than {NOTE_MAX_BYTES} UTF-8 bytes")
    return value


def parse_request(request):
    allowed = {
        "v", "client", "space", "project", "session", "op", "request_id",
        "task_id", "claim_id", "fence", "outcome", "ttl_secs", "note",
        "board_filter", "budget_bytes",
    }
    unknown = sorted(set(request) - allowed)
    if unknown:
        raise BridgeError(f"unknown request fields: {', '.join(unknown)}")
    version = request.get("v")
    if version != SCHEMA_VERSION or isinstance(version, bool) or not isinstance(version, int):
        raise BridgeError(f"v must be the integer {SCHEMA_VERSION}")
    client = request.get("client")
    if client not in CLIENTS:
        raise BridgeError(f"client must be one of {', '.join(CLIENTS)}")
    op = request.get("op")
    if op not in OPERATIONS:
        raise BridgeError(f"op must be one of {', '.join(OPERATIONS)}")
    space = checked_string(request, "space", SPACE_MAX, required=True,
                           pattern=TOKEN_CHARS, label="space")
    session = checked_string(request, "session", SESSION_MAX, required=True)
    project = checked_project(checked_string(request, "project", PROJECT_MAX))
    request_id = checked_string(request, "request_id", REQUEST_ID_MAX,
                                pattern=PRINTABLE_ASCII, label="request_id")
    note = checked_note(request.get("note"))
    ttl = checked_int(request, "ttl_secs", TTL_MIN, TTL_MAX, default=TTL_DEFAULT)
    task_id = checked_int(request, "task_id", 1, 2**63 - 1)
    claim_id = checked_int(request, "claim_id", 1, 2**63 - 1)
    fence = checked_int(request, "fence", 1, 2**63 - 1)
    outcome = request.get("outcome")
    if outcome is not None and outcome not in FINISH_OUTCOMES:
        raise BridgeError(f"outcome must be one of {', '.join(FINISH_OUTCOMES)}")
    budget = checked_int(request, "budget_bytes", BUDGET_MIN, BUDGET_MAX,
                         default=BUDGET_DEFAULT)
    board_filter = request.get("board_filter")
    if board_filter is not None:
        if op != "context":
            raise BridgeError("board_filter is only valid with op=context")
        if not isinstance(board_filter, dict):
            raise BridgeError("board_filter must be an object")
        extra = sorted(set(board_filter) - {"state", "ready", "limit", "offset"})
        if extra:
            raise BridgeError(f"unknown board_filter fields: {', '.join(extra)}")
        state = board_filter.get("state")
        if state is not None and state not in STATES:
            raise BridgeError(f"board_filter.state must be one of {', '.join(STATES)}")
        ready = board_filter.get("ready", False)
        if not isinstance(ready, bool):
            raise BridgeError("board_filter.ready must be a boolean")
        board_filter = {
            "state": state,
            "ready": ready,
            "limit": checked_int(board_filter, "limit", 1, BOARD_LIMIT_MAX,
                                 default=BOARD_LIMIT_DEFAULT),
            "offset": checked_int(board_filter, "offset", 0, 2**63 - 1, default=0),
        }
    if request_id is not None and op not in REQUEST_ID_OPS:
        raise BridgeError(
            f"request_id is only supported for {' and '.join(REQUEST_ID_OPS)}; "
            "renew and release are never idempotent: on an unknown outcome query "
            "(`work verify`/`work claims`) and reconcile, do not blindly repeat")
    if note is not None and op not in NOTE_OPS:
        raise BridgeError(f"note is only valid with op being {' or '.join(NOTE_OPS)}")
    if op not in TTL_OPS and "ttl_secs" in request:
        raise BridgeError(f"ttl_secs is only valid with op being {' or '.join(TTL_OPS)}")
    parsed = {
        "client": client, "op": op, "space": space, "session": session,
        "project": project, "request_id": request_id, "note": note,
        "ttl_secs": ttl, "task_id": task_id, "claim_id": claim_id,
        "fence": fence, "outcome": outcome, "budget_bytes": budget,
        "board_filter": board_filter,
    }
    _require_operation_fields(parsed)
    return parsed


def _require_operation_fields(parsed):
    op = parsed["op"]
    if op == "claim":
        for field in ("task_id", "project"):
            if parsed[field] is None:
                raise BridgeError(f"{op} requires {field}")
    if op in ("renew", "release", "finish"):
        for field in ("claim_id", "fence"):
            if parsed[field] is None:
                raise BridgeError(f"{op} requires {field}")
    if op == "finish" and parsed["outcome"] is None:
        raise BridgeError("finish requires outcome completed|failed")


def resolve_binary():
    """Trusted host configuration, in decreasing order of explicitness.

    The request is never consulted: no amount of request data can steer this
    bridge at another executable.
    """
    config_path = os.environ.get(CONFIG_ENV)
    if config_path:
        try:
            config = json.loads(Path(config_path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ConfigError(f"cannot read {CONFIG_ENV} {config_path}: {error}")
        if not isinstance(config, dict) or not isinstance(config.get("brain_bin"), str):
            raise ConfigError(f"{config_path} must be an object with a brain_bin string")
        candidate = Path(config["brain_bin"]).expanduser()
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            raise ConfigError(f"{candidate} from {config_path} is not an executable file")
        return str(candidate)
    candidate = os.environ.get(BINARY_ENV)
    if candidate:
        path = Path(candidate).expanduser()
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ConfigError(f"{BINARY_ENV} {candidate} is not an executable file")
        return str(path)
    found = shutil.which(BINARY_NAME)
    if not found:
        raise ConfigError(
            f"no {BINARY_NAME} on PATH; set {CONFIG_ENV} or {BINARY_ENV} to the installed binary")
    return found


def build_argv(binary, agent, parsed):
    """The argument array for one coordinator call. Shell never involved."""
    base = [binary, "--space", parsed["space"], "--agent", agent, "work"]
    op = parsed["op"]
    if op == "context":
        argv = base + ["board"]
        board = parsed["board_filter"] or {}
        if parsed["project"]:
            argv += ["--project", parsed["project"]]
        if board.get("state"):
            argv += ["--state", board["state"]]
        if board.get("ready"):
            argv.append("--ready")
        argv += ["--limit", str(board.get("limit", BOARD_LIMIT_DEFAULT)),
                 "--offset", str(board.get("offset", 0))]
        return argv
    if op == "claim":
        argv = base + ["claim", str(parsed["task_id"]), "--ttl", str(parsed["ttl_secs"])]
        if parsed["request_id"]:
            argv += ["--request-id", parsed["request_id"]]
        return argv
    if op == "renew":
        return base + ["renew", str(parsed["claim_id"]), str(parsed["fence"]),
                       "--ttl", str(parsed["ttl_secs"])]
    if op == "release":
        return base + ["release", str(parsed["claim_id"]), str(parsed["fence"]),
                       "--note", parsed["note"] or ""]
    if op == "finish":
        argv = base + ["state", str(parsed["claim_id"]), str(parsed["fence"]),
                       parsed["outcome"]]
        if parsed["note"]:
            argv += ["--note", parsed["note"]]
        if parsed["request_id"]:
            argv += ["--request-id", parsed["request_id"]]
        return argv
    raise BridgeError(f"unsupported op {op}")


def build_show_argv(binary, agent, parsed):
    return [binary, "--space", parsed["space"], "--agent", agent,
            "work", "show", str(parsed["task_id"])]


def _kill_reap(proc):
    """Kill and reap so a capped/over-time child never becomes a daemon."""
    try:
        proc.kill()
    except OSError:
        pass
    try:
        proc.wait(timeout=REAP_GRACE_SECS)
    except Exception:
        pass


def _communicate_bounded(proc, deadline):
    """Read bounded stdout/stderr with a wall deadline.

    Uses non-blocking reads on the real pipes. On timeout or cap overflow the
    child is killed and reaped and nothing partial is returned.
    """
    out_fd = proc.stdout.fileno()
    err_fd = proc.stderr.fileno()
    os.set_blocking(out_fd, False)
    os.set_blocking(err_fd, False)
    bufs = {out_fd: bytearray(), err_fd: bytearray()}
    caps = {out_fd: MAX_STDOUT_BYTES, err_fd: MAX_STDERR_BYTES}
    open_fds = {out_fd, err_fd}
    while open_fds:
        if time.monotonic() > deadline:
            _kill_reap(proc)
            raise TransportError(
                "coordinator call timed out; outcome unknown. For claim/finish "
                "re-send the same request unchanged (same request_id); for "
                "renew/release QUERY the coordinator (work verify / work claims)"
                " and reconcile — do not assume success or blindly retry",
                detail={"timeout_secs": SUBPROCESS_TIMEOUT_SECS})
        made_progress = False
        for fd in list(open_fds):
            try:
                chunk = os.read(fd, 65536)
            except BlockingIOError:
                continue
            made_progress = True
            if not chunk:
                open_fds.discard(fd)
                continue
            bufs[fd] += chunk
            if len(bufs[fd]) > caps[fd]:
                _kill_reap(proc)
                name = "stdout" if fd == out_fd else "stderr"
                raise TransportError(
                    f"coordinator {name} exceeded the {caps[fd]}-byte capture "
                    "cap; outcome unknown and partial output discarded",
                    detail={f"{name}_cap": caps[fd]})
        if not made_progress:
            if proc.poll() is not None and not open_fds:
                break
            time.sleep(0.005)
    # Both pipes are at EOF, but a child that closes them and then hangs must
    # still respect the deadline: wait only for what is left, then kill+reap.
    remaining = deadline - time.monotonic()
    try:
        returncode = proc.wait(timeout=max(0.05, remaining))
    except subprocess.TimeoutExpired:
        _kill_reap(proc)
        raise TransportError(
            "coordinator closed its pipes but did not exit within the "
            "deadline; outcome unknown", detail={"timeout_secs": SUBPROCESS_TIMEOUT_SECS})
    return returncode, bytes(bufs[out_fd]), bytes(bufs[err_fd])


def run_brain(argv, deadline):
    """One coordinator call. Outcome unknown on any transport failure."""
    env = dict(os.environ)
    env["BRAIN_OFFLINE"] = "true"
    started = time.monotonic()
    try:
        proc = subprocess.Popen(
            argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, env=env)
    except OSError as error:
        raise TransportError(f"could not start the coordinator binary: {error}")
    try:
        returncode, stdout_bytes, stderr_bytes = _communicate_bounded(proc, deadline)
    finally:
        if proc.poll() is None:
            _kill_reap(proc)
    duration_ms = int((time.monotonic() - started) * 1000)
    try:
        document = json.loads(stdout_bytes.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise TransportError(
            "coordinator answer was not a JSON document; outcome unknown",
            detail={"exit_code": returncode,
                    "stderr_tail": stderr_bytes.decode("utf-8", "replace")[-400:]})
    if not isinstance(document, dict) or document.get("status") not in ("ok", "error"):
        raise TransportError("coordinator answer has no status ok|error; outcome unknown")
    return document, duration_ms


def _wire_bytes(document):
    """Exact size of what main() emits, trailing newline included."""
    return len((json.dumps(document, ensure_ascii=False, sort_keys=True) + "\n")
               .encode("utf-8"))


def _shrink_context_step(payload):
    """One reduction, coarse-enough-first. Returns False when nothing remains.

    Text is cut only on Unicode code point boundaries, so the JSON stays valid.
    """
    board = payload.get("board") or {}
    tasks = board.get("tasks")
    if tasks:
        tasks.pop()
        payload["omitted_board_tasks"] = payload.get("omitted_board_tasks", 0) + 1
        return True
    packet = payload.get("task")
    if isinstance(packet, dict) and not packet.get("omitted"):
        task = packet.get("task") or {}
        for field in ("detail", "title"):
            text = task.get(field)
            if isinstance(text, str) and text:
                task[field] = ""
                packet.setdefault("truncated_fields", []).append(field)
                return True
        events = packet.get("recent_events")
        if events:
            events.pop()
            payload["omitted_events"] = payload.get("omitted_events", 0) + 1
            return True
        payload["task"] = {"omitted": True,
                           "task_id": task.get("id", payload.get("task_id"))}
        return True
    if payload.get("references"):
        payload["references"] = {"omitted": True}
        return True
    return False


def enforce_budget(response, budget_bytes):
    """Trim the context response until the WHOLE emitted document fits.

    Envelope fields, escaping, truncation markers and the newline all count,
    because the measurement uses the exact emit encoding. If nothing smaller
    is achievable, answer with a minimal budget_exceeded refusal instead of
    pretending the budget was met. budget_bytes >= 1024 always fits that
    minimal document (its fixed fields total far less than 1024 bytes).
    """
    truncated = False
    payload = response.get("result")
    if isinstance(payload, dict):
        while _wire_bytes(response) > budget_bytes and _shrink_context_step(payload):
            truncated = True
    if _wire_bytes(response) > budget_bytes:
        response["result"] = {"omitted": True,
                              "reason": "context does not fit the declared budget_bytes",
                              "budget_bytes": budget_bytes}
        response["status"] = STATUS_REFUSED
        response["outcome_kind"] = "budget_exceeded"
        truncated = True
    response["truncated"] = truncated or bool(response.get("truncated"))
    # Invariant: with budget_bytes >= BUDGET_MIN this document always fits.
    if _wire_bytes(response) > budget_bytes:
        minimal = {"v": SCHEMA_VERSION, "status": STATUS_REFUSED,
                   "op": "context", "outcome_kind": "budget_exceeded",
                   "truncated": True,
                   "result": {"omitted": True, "budget_bytes": budget_bytes},
                   "offline": True, "network_capable": False}
        return minimal
    return response


def envelope(parsed, agent, status, outcome_kind=None, result=None, **extra):
    document = {
        "v": SCHEMA_VERSION,
        "status": status,
        "agent": agent,
        "client": parsed["client"],
        "space": parsed["space"],
        "session_hash12": agent.split("-", 1)[1],
        "op": parsed["op"],
        "outcome_kind": outcome_kind,
        "result": result,
    }
    document.update(extra)
    return document


def verify_claim_scope(binary, agent, parsed, deadline):
    """Re-read the task before claiming; refuse when the project mismatches.

    The coordinator records the task's project at creation and has no command
    that mutates it, so this read-only guard is exact (the residual
    check-then-act window cannot change a project, only lifecycle state, and
    the claim itself re-checks that inside its transaction).
    """
    show_doc, _ = run_brain(build_show_argv(binary, agent, parsed), deadline)
    if show_doc["status"] != "ok":
        return show_doc
    task = show_doc.get("task") or {}
    actual = task.get("project")
    if actual != parsed["project"]:
        return {
            "status": "error",
            "error_kind": "project_mismatch",
            "hint": (f"task {parsed['task_id']} belongs to project "
                     f"{actual!r} in space {parsed['space']!r}, not "
                     f"{parsed['project']!r}; refusing to claim it under the "
                     "wrong project"),
            "details": {"task_id": parsed["task_id"],
                        "task_project": actual,
                        "requested_project": parsed["project"]},
        }
    return None


def handle_context(binary, agent, parsed, deadline):
    board_doc, board_ms = run_brain(build_argv(binary, agent, parsed), deadline)
    if board_doc["status"] != "ok":
        return board_doc, board_ms, board_doc.get("error_kind")
    payload = {
        "board": {
            "space": board_doc.get("space"),
            "project": board_doc.get("project"),
            "tasks": board_doc.get("tasks", []),
            "counts": board_doc.get("counts"),
            "has_more": board_doc.get("has_more"),
            "next_offset": board_doc.get("next_offset"),
            "server_time": board_doc.get("server_time"),
        },
        "references": {
            "memory": ("search/read memory through the workspace MCP tools "
                       "memory_search and memory_read; ask for them by name"),
            "archive": ("the private conversation archive is reachable only "
                        "through archive_status/archive_search/archive_read "
                        "and is never included here"),
        },
        "trust": board_doc.get("trust"),
        "data_is_agent_supplied": True,
    }
    duration_ms = board_ms
    if parsed["task_id"] is not None:
        show_doc, show_ms = run_brain(build_show_argv(binary, agent, parsed), deadline)
        duration_ms += show_ms
        if show_doc["status"] == "ok":
            payload["task"] = show_doc
        else:
            payload["task"] = {"omitted": True, "task_id": parsed["task_id"],
                               "error_kind": show_doc.get("error_kind"),
                               "hint": show_doc.get("hint")}
    return payload, duration_ms, None


def handle_write(binary, agent, parsed, deadline):
    if parsed["op"] == "claim":
        scope_error = verify_claim_scope(binary, agent, parsed, deadline)
        if scope_error is not None:
            # Either the project guard tripped (project_mismatch) or the task
            # re-read itself failed; both are error documents to forward.
            return scope_error, 0
    document, duration_ms = run_brain(build_argv(binary, agent, parsed), deadline)
    return document, duration_ms


def main(argv=None, stream=None):
    stream = stream or sys.stdin
    started = time.monotonic()
    deadline = started + SUBPROCESS_TIMEOUT_SECS + 3.0
    parsed = None
    agent = None
    try:
        parsed = parse_request(read_request(stream))
        agent = derive_agent(parsed["client"], parsed["session"])
        binary = resolve_binary()
        if parsed["op"] == "context":
            payload, duration_ms, error_kind = handle_context(binary, agent, parsed, deadline)
            if error_kind:
                response = envelope(parsed, agent, STATUS_REFUSED, outcome_kind=error_kind,
                                    result=payload, truncated=False,
                                    duration_ms=duration_ms,
                                    limits={"budget_bytes": parsed["budget_bytes"],
                                            "max_board_tasks": BOARD_LIMIT_MAX,
                                            "subprocess_timeout_secs": SUBPROCESS_TIMEOUT_SECS})
            else:
                response = envelope(parsed, agent, STATUS_OK,
                                    result=payload, truncated=False,
                                    duration_ms=duration_ms,
                                    limits={"budget_bytes": parsed["budget_bytes"],
                                            "max_board_tasks": BOARD_LIMIT_MAX,
                                            "subprocess_timeout_secs": SUBPROCESS_TIMEOUT_SECS})
            response = enforce_budget(response, parsed["budget_bytes"])
        else:
            document, duration_ms = handle_write(binary, agent, parsed, deadline)
            status = STATUS_OK if document["status"] == "ok" else STATUS_REFUSED
            response = envelope(parsed, agent, status, outcome_kind=document.get("error_kind"),
                                result=document, duration_ms=duration_ms,
                                limits={"ttl_secs": [TTL_MIN, TTL_MAX],
                                        "note_bytes": NOTE_MAX_BYTES,
                                        "subprocess_timeout_secs": SUBPROCESS_TIMEOUT_SECS})
    except BridgeError as error:
        fallback = parsed or {"client": None, "space": None, "op": None}
        response = envelope(fallback, agent or "-", STATUS_INVALID,
                            outcome_kind="invalid_input", result={"hint": error.hint})
    except ConfigError as error:
        fallback = parsed or {"client": None, "space": None, "op": None}
        response = envelope(fallback, agent or "-", STATUS_CONFIG,
                            outcome_kind="not_configured", result={"hint": str(error)})
    except TransportError as error:
        result = {"hint": error.hint}
        if error.detail:
            result["details"] = error.detail
        response = envelope(parsed, agent, STATUS_TRANSPORT,
                            outcome_kind="unavailable", result=result)
    response["offline"] = True
    response["network_capable"] = False
    out = json.dumps(response, ensure_ascii=False, sort_keys=True)
    sys.stdout.write(out + "\n")
    sys.stdout.flush()
    return 0 if response["status"] in (STATUS_OK, STATUS_REFUSED) else 1


if __name__ == "__main__":
    sys.exit(main())
