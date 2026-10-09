# Agent lifecycle bridge

Provider-neutral, source-only helper that lets Codex, Claude Code and Hermes
use the Brain work coordinator through one strict contract. It is offline by
default, makes no provider/model calls, installs no hooks, starts no daemons.

    python3 tools/agent_bridge.py <<'EOF'
    {"v":1,"client":"codex","space":"default","project":"proj",
     "session":"sess-1","op":"claim","task_id":7,"request_id":"c-1"}
    EOF

## Trust model (read before wiring anything)

* **Binary is host config, never request data.** Resolution order: the JSON
  file named by `BRAIN_BRIDGE_CONFIG` (`{"brain_bin": "/abs/path/brain"}`),
  then `BRAIN_BINARY`, then `brain` on PATH. A request cannot change the
  executable, database, agent or machine identity. Machine identity is the
  coordinator's own installation id (`$BRAIN_HOME/machine-id`), or the host's
  `BRAIN_MACHINE_ID`; never supplied per-request.
* **Agent identity is derived, not chosen.** `--agent` is
  `client-hash12(session)` (sha-256 of the raw session tag, first 12 hex).
  Two sessions of one client are different coordinatorside agents; the raw
  session tag is hashed and never sent. A future network credential MUST bind
  this exact identity; this local helper does not yet do network auth.
* **`shell=False`, argument arrays only.** Request strings reach the binary
  only as literal argv elements (title, note, project) or as validated
  integers. Nothing is pasted into a shell or a flag name.

## Identity, lease, task lifecycle

The bridge is a thin, faithful adapter over the authoritative coordinator
(`brain work`, schema in `brain-core/src/coordination.rs`, contract in
`docs/coordination.md`). It invents no coordination semantics of its own.

1. `context` — read a bounded board page (optional `board_filter`:
   `{"state","ready","limit","offset"}`) and, when `task_id` is given, that
   task's packet. The answer includes a `references` note pointing at the MCP
   `memory_search`/`memory_read` and the archive tools, and the coordinator's
   `trust` label. The conversation archive is NEVER included.
2. `claim` — takes a lease on one task. Before claiming it re-reads the task
   (read-only) and refuses `project_mismatch` when the caller's `project` does
   not equal the task's authoritative project — so a session cannot believe it
   is claiming project A while grabbing a task that lives in project B.
   Optional `request_id` makes retries replay.
3. `renew` — extend with the current fencing token.
4. `finish` — `outcome` is ONLY `completed` or `failed`.
5. `release` — give the claim up, no state change.

### Retry safety is NOT uniform (this matters)

* `claim` and `finish` accept `request_id`. On a lost answer, re-sending the
  SAME request replays the coordinator's stored answer (`"replayed": true`).
  These two are idempotent.
* `renew` and `release` accept NO `request_id` on the coordinator and the
  bridge refuses one. On a `transport_unknown` outcome for these, do NOT
  blindly repeat. First QUERY: `brain work verify <claim> <fence>` or
  `brain work claims`, reconcile against the coordinator, then act.

## Failure vocabulary

`status` is `ok`, `refused`, `invalid`, `transport_unknown`, or
`config_error`. `outcome_kind` is the coordinator's `error_kind` forwarded
unchanged (`busy`, `not_owner`, `stale_fence`, `lease_expired`,
`invalid_input`, `not_found`, `not_configured`, `unavailable`, ...) or a
bridge-local kind: `project_mismatch` (claim project guard), `budget_exceeded`
(context could not fit). No `transport_unknown` is ever reported as success.

## Bounded output

`budget_bytes` (1024..65536, default 16000) caps the WHOLE emitted UTF-8 JSON
document — envelope, escaping, truncation markers and the trailing newline
included — measured with the exact emit encoding, not a compact estimate. The
context payload is shrunk (whole board tasks first, then task detail/title cut
on code-point boundaries, then events) and every reduction is reported via
`truncated` / `omitted_board_tasks` / `omitted_events`. If the document still
cannot fit, the answer is a minimal `budget_exceeded` refusal; the bridge
never claims a budget it did not meet.

Subprocess capture is bounded: stdout ≤ 1 MiB, stderr ≤ 16 KiB, with a
20 s wall deadline; a child that floods or hangs is killed and reaped (never
left as a daemon) and the outcome reported `transport_unknown`.

## Input strictness

Unknown request fields are refused. `task_id`/`claim_id`/`fence`/`ttl_secs`/
`budget_bytes` reject booleans, floats, strings and out-of-range values.
`note` is bounded in UTF-8 BYTES (2000). `project` is ASCII
`[A-Za-z0-9._-/]` with no empty/`.`/`..` segments; `space` matches the
coordinator token rule. A JSON string containing a lone surrogate escape
(e.g. `"\ud800"`) is rejected `invalid` BEFORE any hashing, serialization or
UTF-8 encoding, instead of crashing.

## Provider wiring: supported vs proposed

Native three-client pilot is PENDING. The bridge is not installed into any
client; everything below is documentation, not a claim of a working hook.

IMPORTANT correction to earlier notes: Codex is NOT notify-only. Installed
codex-cli 0.149.1 `codex features list` reports `hooks stable true`
(`plugin_hooks` is removed). Verified from the installed CLI this slice, not
from web browsing.

| Client | Verified locally | Proposal |
|---|---|---|
| Codex | `codex features list` → `hooks stable true` | map SessionStart → `context`, Stop → `release`/`finish` via `tools/agent_bridge.py`. Hook source of truth: `codex-rs/hooks/src/lib.rs`, `session_start.rs`. Native behavior UNTESTED. |
| Claude Code | hook contract from docs (`code.claude.com/docs/en/hooks`) | SessionStart → `context`, SessionEnd/Stop → `release`/`finish`. Config is EXAMPLE-only below; not installed. |
| Hermes | `brain serve` MCP stdio is the native path | prefer MCP `work_claim`/`work_renew`/`work_set_state` directly (per `docs/coordination.md`); bridge is for CLI-shaped lifecycle glue. |

Do not install hooks from this slice. Configuration EXAMPLES (edit paths,
verify with `brain init --agents --dry-run` before anything real):

```json
// Claude Code settings.json (EXAMPLE, not installed)
{"hooks": {"SessionStart": [{"command": "python3 tools/agent_bridge.py"}]}}
```

## Not in this helper

* No network transport or remote credentials (local coordinator only).
* No real provider integration; the three-client native pilot is the next gate.
* No automatic enforcement of file edits; coordination stays cooperative.
