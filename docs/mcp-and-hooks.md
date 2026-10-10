# MCP, hooks, and session brief

`brain serve` is a newline-delimited JSON-RPC 2.0 MCP server over stdio. It
supports initialization, ping, `tools/list`, and `tools/call`. The default server exposes 27 tools: thirteen memory/archive tools listed
below, ten `work_*` tools (see [coordination.md](coordination.md)), and
`map_status`, `map_timeline`, `map_packet`, `map_ingest_coordinator` (see
[graph-explorer.md](graph-explorer.md)). An explicitly configured remote
MCP client adds the five read tools described in [remote-mcp.md](remote-mcp.md).
The memory/archive tools are:

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

### Relocated client homes (not followed)

Claude Code (`CLAUDE_CONFIG_DIR`), Codex (`CODEX_HOME`) and Hermes
(`HERMES_HOME`) can keep their configuration outside the default `~/.claude`,
`~/.codex` and `~/.hermes`. Brain only writes and checks the default
locations, so it classifies each variable from its raw value in the process
environment, without opening, stat'ing or resolving the path:

| state | meaning |
|---|---|
| `unset` | variable not set; legacy behaviour |
| `empty` | set to the empty string; treated as the default |
| `default_equivalent` | an absolute path lexically equal to the default (`//`, `/./`, trailing `/`) |
| `unsupported_override` | an absolute path elsewhere |
| `unresolved_override` | relative, non-UTF-8 or containing `..` |

For the last two states `brain init --agents` (also with `--yes` or
`--dry-run`) does not detect, read or write any of that client's default
files, including `~/.claude.json` for Claude Code, and prints one
`CONFLICT` line naming the variable and the state, never its value. Other
clients and the stable binary copy are installed as before. Brain does not
write to the relocated location. `brain uninstall` (also `--dry-run`) skips
the manifest entries for a relocated client's exact default files before
reading the current file or its backup, prints the same `CONFLICT` line, and
restores the other clients' entries. While entries are skipped, the manifest,
its backups and the stable binary are kept, so a later uninstall with the
variable unset can still restore them. That later uninstall treats an entry
as already restored, without rewriting it, when the file holds exactly its
pre-install bytes again (or, for a file Brain created, is absent: only a
`NotFound`, never an unreadable file). Any other difference stays a
`CONFLICT` and keeps the manifest and backups. Every backup that will be used
is checked against its recorded SHA-256 before any file is changed. The
dry-run count includes only entries that would really be restored now. Entries
are matched against paths under the current `HOME`; if `HOME` changed since
the install, including a different spelling or symlink alias for the same
directory, old paths outside the current allowlist refuse the whole uninstall,
and Brain does not try to locate the old home.

`brain doctor` adds three fields to each client entry:

- `config_location`: `{variable, state}`, without the value.
- `mcp_support` and `hook_support`: `supported`, `relocated_not_followed` or,
  for Hermes, `unsupported`, since Brain has no Hermes MCP or hook
  integration.

For a relocated client, `mcp_config_entry_present` and
`hook_config_marker_present` stay `null` (unknown), whatever stale markers
the default files contain, and `misplaced_claude_mcp_entry` is `false`
because those files are not read. None of these fields claims a handshake
(`runtime_handshake` stays `not_checked`). No conversation discovery or
capture is moved to the relocated location.

### What `brain uninstall` checks before acting

`brain uninstall` acts only on what `brain init --agents` itself recorded.
Before it reads or changes any client file or backup, it refuses the whole run
(nothing is changed) unless all of the following hold:

- The Brain home and any existing `install-backups` are real directories, not symlinks,
  owned by you, with no group or other access.
- The manifest is a regular, owner-only file of at most 1 MiB, read through
  a handle that does not follow a final symlink.
- The manifest is schema version 1 with no unknown fields.
- Every entry names one of the exact default Claude Code, Codex or Cursor
  files under the current `HOME`, with no duplicates.
- Every SHA-256 is 64 lowercase hex characters.
- `existed`, `before_sha256` and `backup` agree with each other.
- Every backup name is exactly three ASCII digits plus `.bak`, is unique, and
  comes from the installer's plan numbering. Gaps in that numbering are
  normal.
- No recorded mode has setuid, setgid or sticky bits.

Each backup it needs is read the same way, owner-only and bounded. A missing
Brain home is a no-op. With an existing private, owned Brain home, a missing
manifest is also a no-op. An existing Brain home that is shared, permissive or
a symlink refuses uninstall even when it has no manifest; no files are changed.
A missing
`install-backups` is accepted only when no backup has to be read.

`HOME` itself may be a symlink. A symlink anywhere below it on the way to a
client file, including the file itself, makes that entry a `CONFLICT`:
nothing behind the symlink is read or written, no client directory is
chmodded, and the manifest, backups and binary are kept.

These checks stop a stray or tampered manifest from steering uninstall to
unrelated files. They do not make uninstall safe against a process with write
access to the same directories that changes files between checks and writes (TOCTOU);
no such guarantee is claimed.

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
