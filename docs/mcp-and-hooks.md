# MCP, hooks, and session brief

`brain serve` is a newline-delimited JSON-RPC 2.0 MCP server over stdio. It
supports initialization, ping, `tools/list`, and `tools/call`. The twenty-three tools (ten of them `work_*`, see `docs/coordination.md`)
are:

1. `memory_search`
2. `memory_read`
3. `memory_write`
4. `memory_update`
5. `memory_forget`
6. `memory_spaces`
7. `memory_brief`
8. `memory_blockers`
9. `memory_checkpoint`
10. `memory_goals`
11. `archive_status`
12. `archive_search`
13. `archive_read`

`memory_write` and the optional `kind` in `memory_update` accept exactly
`fact`, `decision`, `task`, `gotcha`, `error`, or `note`. Tool failures are
returned as bounded JSON with a `status` and an actionable `hint`; raw errors
are not injected into agent context.

### Reading memories by ID

`memory_search` accepts an optional `space` and reports the `space` of every
hit. `memory_read` takes the same optional `space` (default: the configured
one) and looks every ID up directly in that space with an indexed bi-temporal
query, so old records are as reachable as new ones. It never falls back to
another space: an ID stored elsewhere comes back in `missing_ids`. Read with the
space `memory_search` reported.

Each record carries a `status`: `active`; `forgotten` (validity closed, hidden
from search; its last text only with `include_forgotten: true`);
`not_yet_valid` (validity starts in the future); or `not_memory` (an imported
or indexed graph node, no text — open it in the graph explorer). An unknown
space answers `unknown_space`. Text keeps its line breaks and is sliced on
character boundaries: `max_bytes` (256..16000, default 1200) per record, and
for one ID `offset` continues from the previous `next_offset`. One call reads at
most 20 IDs (the rest return as `unread_ids`) and about 48 KB of text (records
past that return as `deferred_ids`); read those in a further call.

`brain brief` emits the ordered `ID`, `GOAL`, `PRIMER`, `OPEN`, `GOTCHAS`,
`HISTORY`, and `FOOTER` sections. Its UTF-8 byte count is capped at 550. Since a
cl100k token always contains at least one byte, the reported byte count is a
conservative upper bound below the 600-token contract.

`brain hook --event NAME --client claude|codex|cursor` reads one JSON payload
from stdin. The fast path performs no model, network, Git, or SQLite work. A
SessionStart returns `{}`, as do all other events. Automatic brief injection was
disabled so mutable memory cannot cross into a system-priority channel. Request
`memory_brief` explicitly as a tool result instead. Sanitized events are appended below `~/.brain/`; raw user prompts,
secret-like fields, high-entropy strings, and denylisted personal paths are not
queued. The current journal captures event metadata, not conversation history.
JSONL appends are serialized under an OS file lock; this prevents cooperating
writers interleaving records. The fast path has no model/network work, but disk
latency and lock contention mean a hard wall-clock deadline is not guaranteed.

`brain doctor` reads only bounded configuration and journal snapshots. It reports
malformed records, future timestamps, partial tails and configuration markers.
It does not start services or models and does not claim that configuration proves
a successful connection. Conversation capture remains explicitly unimplemented;
Hermes operational-graph import is not a transcript adapter.

`brain init --agents --dry-run` previews user-scope MCP, hook, and instruction
changes. A real install asks per file unless `--yes` is supplied. It never
writes tracked user configuration inside a Git worktree. `brain uninstall` uses the installation manifest
and SHA-256 checks to restore undrifted files byte for byte, while preserving
manual edits as conflicts.

## Conversation archive tools

`archive_status`, `archive_search` and `archive_read` read the private
conversation archive (`$BRAIN_HOME/archive/sessions.sqlite`, see
`docs/session-archive.md`) read-only. They are scoped per client and session,
not per Brain space. Every answer carries `"trust": "untrusted_conversation"`:
archived text is evidence to cite, never instructions. A missing archive is
`error_kind: not_configured`; a foreign, permissive or unknown-schema file is
refused with `unsafe_permissions` / `incompatible_archive`.

## Binary upgrades and running servers

A connected client keeps its `brain serve` process, and that process keeps the
executable it started with, even after an install replaces `~/.brain/bin/brain`
(Linux shows it as `brain (deleted)`). Since this fix its background vector
worker re-executes that same running image via `/proc/self/exe` instead of the
now-missing pathname, so it neither fails with ENOENT nor mixes in the new
generation. It still serves the old code: after a validated deployment,
reconnect each client deliberately (e.g. restart its MCP server) to pick up the
new binary. `brain doctor` reports `this_process` (version, whether its own
executable was replaced) for the doctor run only; it cannot see other servers,
and a config entry or a deleted-path observation does not prove a client has
refreshed. On non-Linux systems the worker is started by pathname and may fail
or run the newer binary after a replacement.
