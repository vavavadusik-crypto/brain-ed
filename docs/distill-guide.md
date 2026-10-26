# Distillation in 15 minutes

Distillation is an optional step that reads the already-collected conversation
archive (`brain archive`) and extracts **durable facts** — decisions, errors,
dead ends, findings and plans — into the memory graph. The archive stays a
private conversation store; the distiller only selects sessions, sends their
masked text to a provider, and records the facts that pass validation. The full
reference lives in [docs/distiller.md](distiller.md); this is a working route
with verified real output.

Полная русская версия: [docs/distill-guide.ru.md](distill-guide.ru.md).

Everything below was verified on a synthetic archive in a temporary HOME with a
fake OpenAI-compatible provider (script at the end); no real model was called
and no real archive was read.

## 1. Providers

A provider turns session text into facts. Configure one once:

```console
$ brain llm add local --base-url http://127.0.0.1:8080 --model local-model --context-tokens 16384
```

Three kinds of providers; the difference is safety:

| Kind | What it is | Guarantees |
|---|---|---|
| Local (llama.cpp and compatible) | HTTP on `127.0.0.1` | the text never leaves the machine; secrets are masked before sending anyway |
| HTTPS gateway | a cloud OpenAI-compatible API | the text goes to the provider under its terms; the key comes only through `api_key_env` (the name of an environment variable; the secret itself is never written to the config) |
| `claude` subscription | a CLI provider through a Claude subscription | the provider has **no tools** — it cannot execute anything, only return a JSON of facts |
| `codex` subscription | a CLI provider through a Codex subscription | the provider runs in a **read-only sandbox** — it can read, never write or execute |

For sensitive archives prefer a local model: masking removes recognized secrets
but not all private content.

## 2. A synthetic archive and collection

```console
$ brain --offline init
{ "database": "…/.brain/brain.sqlite", "schema_version": 5, "status": "initialized" }

$ brain archive enable --since-days 7
$ brain archive collect
{ "status": "ok", "stored_entries": { "claude": 4 }, … }
```

`collect` is the same bounded run the timer performs: it reads new conversation
files, masks secrets and stores entries in `archive/sessions.sqlite`.

## 3. The first run

```console
$ brain distill run --dry-run
{
  "accepted": 0,
  "bytes": 520,
  "calls": 1,
  "chunks": 1,
  "facts_written": 0,
  "proposed": 0,
  "rejected": {},
  "roles": ["assistant", "tool_error", "user"],
  "sessions": 1,
  "status": "dry_run"
}
```

`--dry-run` prepares exactly the same requests (bytes/chunks) but writes nothing
and calls nobody.

```console
$ brain distill run --session synth-4
distill session=1 chunk=1 bytes=262 state=started
distill session=1 chunk=1 bytes=262 seconds=0.530 proposed=2 accepted=2 rejected={} state=finished
{
  "accepted": 2,
  "bytes": 262,
  "calls": 1,
  "chunks": 1,
  "facts_written": 2,
  "proposed": 2,
  "rejected": {},
  "roles": ["assistant", "tool_error", "user"],
  "sessions": 1,
  "status": "ok"
}
```

`proposed` — facts the provider returned; `accepted` — facts that passed
validation (see §5); `facts_written` — facts written to the graph. Progress is
kept in the database: re-running the same session reports `sessions: 0`,
re-reads nothing and duplicates nothing.

## 4. Useful flags

```console
# keep the provider's raw answer in a directory (0600) for debugging rejections
$ brain distill run --session synth-5 --keep-raw $T/raw
{ … "accepted": 2, "facts_written": 2, … }
$ ls $T/raw
chunk-3771822-1793040081216924877.json

# only collect sessions that have entries with these roles
$ brain distill run --roles user,assistant,tool_error

# bound the request: at most N sessions per chunk
$ brain distill run --batch-sessions 10 --max-sessions 20 --oldest-first

# a provider list is the fallback order: an unavailable first one is skipped
$ brain distill run --session synth-6 --provider broken,fake
distill session=1 chunk=1 bytes=253 state=started
distill session=1 chunk=1 bytes=253 seconds=0.566 proposed=2 accepted=2 rejected={} state=finished
{ … "accepted": 2, "facts_written": 2, "status": "ok" }
```

`--provider broken,fake` is a real run: `broken` listened on a dead port, the
distiller moved on to `fake` and recorded the facts.

## 5. Fact validation and rejections

Every proposed fact passes schema and coherence checks; rejections are counted
by reason. Real runs with bad facts from the fake provider:

```console
{ "proposed": 2, "accepted": 0,
  "rejected": { "session_mismatch": 2 }, "status": "ok" }
{ "proposed": 4, "accepted": 0,
  "rejected": { "session_mismatch": 1, "unknown_status": 3 }, "status": "ok" }
```

Reasons (all visible in `rejected` and in the progress line): `session_mismatch`
— the evidence points at another session; `unknown_status` — the status is not
one of the type's list (error: suspected/confirmed/fixed/…, decision:
proposed/adopted/…); plus `over_max_facts`, `unknown_type`, `empty_what`,
`field_too_long`, `no_evidence`, `tool_result_only`, `mixed_projects`. The
progress line states the total:

```console
Error: distillation provider unavailable or returned invalid JSON (transport
error: Network Error); check provider timeout_seconds for slow local models;
cursor not advanced
```

A transport failure does not advance the cursor — simply repeat the run. The
`provider_limited` state means the provider hit its response token limit:
lower `--batch-sessions` (fewer sessions per chunk) or switch to a provider
with a larger window. Another `distill run` continues from the cursor.

## 6. Status and search

```console
$ brain distill status
{
  "completed_sessions": 4,
  "facts": { "decision:adopted": 1, "error:suspected": 1 },
  "remaining_sessions": 0,
  "status": "ok"
}

$ brain --offline ask "Synthetic failure in synth-4" --no-archive
{ … "found": 5, "results": [ { "kind": "error", "score": "0.5161…",
  "source": "archive-session://synth-4" }, … ] }
```

Distilled facts are ordinary graph nodes: `ask` finds them, they carry
`has_session`/`caused_by` relations and bi-temporal fields.

## 7. Scheduling (optional)

The timer is installed through systemd user; here it was verified with a fake
`systemctl` on PATH (your run will enable the real unit):

```console
$ brain distill schedule enable --provider local --every 6h --quiet-hours 23-7
{
  "status": "ok",
  "systemctl_daemon_reload": true,
  "systemctl_enable_or_disable": true,
  "unit_files": ["…/.config/systemd/user/brain-distill.service",
                 "…/.config/systemd/user/brain-distill.timer"]
}

$ brain distill schedule status
{ "active": true, "enabled": true, "last_run": null, "status": "ok",
  "unit_files_installed": true }

$ brain distill schedule disable
```

`--quiet-hours 23-7` is a local clock window (start inclusive, end exclusive) in
which the provider is not called. Changing the provider or period is another
`enable` with the new values.

---

## Appendix: a fake provider for a run without any model

```python
#!/usr/bin/env python3
# fake-provider.py PORT — an OpenAI-compatible server that answers every
# request with one fact per archive entry (the distiller's contract).
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

def fact(eid, session, kind, status, what):
    return {"type": kind, "what": what, "why": "Observed synthetic evidence.",
            "status": status, "session": session, "project": "synthetic",
            "evidence": [eid],
            "status_evidence": {"entry_id": eid, "quote": "synthetic"}}

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        prompt = json.loads(self.rfile.read(n))["messages"][0]["content"]
        data = json.loads(prompt.split("Session data: ", 1)[1])
        facts = []
        for s in data["sessions"]:
            sid = s["session"]
            for i, e in enumerate(s["entries"]):
                if i == 0:
                    facts.append(fact(int(e["id"]), sid, "error", "suspected",
                                      f"Synthetic failure in {sid}"))
                else:
                    f = fact(int(e["id"]), sid, "decision", "adopted",
                             f"Synthetic remediation for {sid}")
                    f["links"] = [{"relation": "caused_by", "target_type": "error",
                                   "target_what": f"Synthetic failure in {sid}"}]
                    facts.append(f)
        body = json.dumps({"choices": [{"message":
            {"content": json.dumps({"facts": facts})}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
```

Run it with `python3 fake-provider.py 48971 &`, then
`brain llm add fake --base-url http://127.0.0.1:48971 --model synthetic --context-tokens 8192`.
