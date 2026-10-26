# MCP, hooks, and session brief

`brain serve` is a newline-delimited JSON-RPC 2.0 MCP server over stdio. It
supports initialization, ping, `tools/list`, and `tools/call`. The default server exposes 27 tools: thirteen memory/archive tools listed
below, ten `work_*` tools (see [coordination.md](coordination.md)), and
`map_status`, `map_timeline`, `map_packet`, `map_ingest_coordinator` (see
[graph-explorer.md](graph-explorer.md)). An explicitly configured remote
MCP client adds the five read tools described in [remote-mcp.md](remote-mcp.md).
When team mode is configured, three additional read-only tools are exposed:
`team_ask`, `team_manifest` and `team_peers`. Their arguments, trust labels and
configuration boundaries are described in [team facts](team-mode-facts.md).

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

`brain hook --event NAME --client claude|codex|cursor` reads one bounded JSON
payload from stdin. It does not run a model or network request. For Claude Code
and Codex, SessionStart returns a fixed memory notice when `session_start_notice`
is enabled (default). Setting `session_start_brief` to `brief` explicitly adds
an available cached project brief, bounded by the project's 2000-token estimate;
no cache rebuild or archive text is injected. Cached records are marked untrusted
evidence. Other responses are `{}`. Activity metadata is sanitized and recorded
best-effort. Conversation/journal collection is a separate opt-in subsystem:
see [session archive](session-archive.md) and [journal](journal.md).

### Hook process contract

The hook writes one JSON response followed by a newline to stdout, nothing to
stderr, and exits 0. This is `{}` except for the SessionStart response above.
Command-line usage errors
(an unknown `--client`, a missing `--event`) still fail as normal CLI errors.

- Input is bytes. Only the first 256 KiB are kept and parsed. Empty input,
  invalid UTF-8, JSON that is malformed or not an object, and input over
  256 KiB all discard the metadata; nothing is journaled.
- Stdin is read through a non-blocking descriptor under a 2 second deadline.
  Oversized input is drained until it exceeds a 16 MiB threshold, so a client
  writing a finite larger payload with `write_all` can finish without EPIPE.
  Reads use 64 KiB chunks: the last chunk can cross that threshold by at most
  64 KiB. Once exceeded, or after the deadline (an idle open pipe or socket,
  for example), the hook stops reading and exits, and the writer may see
  EPIPE. There is no "no EPIPE" promise past the threshold or deadline. No
  helper thread is left waiting on stdin.
- Supported stdin kinds (Linux only):
  - a pipe or regular file is reopened through `/proc/self/fd/0` as a separate
    open file description with its own `O_NONBLOCK`;
  - a Unix **stream** socket (`SOCK_STREAM`, as Node.js `child_process.spawn`
    with `stdio: 'pipe'` and `socketpair` launchers provide) cannot be
    reopened that way (`ENXIO`). The hook duplicates fd 0 and reads it with
    `recv(..., MSG_DONTWAIT)`. That flag applies per call: the hook never calls
    `F_SETFL`, so the open file description it shares with the launcher keeps
    exactly its flags. The same retention limit, drain threshold and deadline apply;
  - anything else (closed stdin, datagram or other sockets, TTYs that cannot
    be reopened) is treated as no input: `{}` is still returned and nothing is
    journaled.
  These launch shapes are verified with synthetic launchers (pipe and
  `UnixStream::pair` in the contract tests). That is not proof that a given
  agent client invokes hooks this way, nor that every event is captured.
  Non-Linux platforms do not read hook input in this preview. On Linux,
  reopening pipes and files requires `/proc`; the stream-socket fallback
  uses only the inherited descriptor.
- `HOME` must be absolute and control-free. When it is unset (the Brain home
  becomes `./.brain`), empty (`.brain`), relative or contains a control
  character, the hook writes no activity anywhere, in particular not into a
  project's own `.brain` below the client's working directory. It still
  answers `{}`. This check runs before any metadata of the Brain home is read.
- Payload paths such as `transcript_path` and `cwd` are untrusted strings. They
  are never opened. Client configuration, transcripts, auth files and history
  are not read either.
- `~/.brain` must already exist as a real directory owned by the current user,
  with no group or other permission bits, inside a parent that is not world-writable
  and permits group writes only for a verified owner-private group. Otherwise the event is dropped. The hook never creates,
  chmods or follows a Brain home. Run `brain init` to create one.
- `hook-events.jsonl` and `hook-dedupe.json` are checked with `lstat` before
  any content is opened. Each must be a regular file owned by the user, with
  mode 0600 or tighter, a single hard link and a bounded size. They are then
  opened with `O_NOFOLLOW|O_NONBLOCK` and re-checked by inode. Symlinks, FIFOs,
  hard links and permissive modes are left exactly as found, and their targets
  are never truncated or chmodded.
- The journal holds one JSON event per line. Each line is at most 8 KiB and the
  file at most 64 MiB. Appends take a non-blocking OS file lock, retried for at
  most 1 second; if the lock stays held, the event is dropped instead of hanging
  the agent. The dedupe state is a small hash and a timestamp, replaced
  atomically through a private temporary file and a rename.

Worst-case hook latency is therefore bounded by about 3 seconds plus process
start-up and disk latency. This is a bounded best-effort journal, not complete
capture. It is not evidence that any client actually invoked the hook.

`brain doctor` reads only bounded configuration and journal snapshots. It reports
malformed records, future timestamps, partial tails and configuration markers.
It does not start services or models and does not claim that configuration proves
a successful connection. Conversation capture is opt-in and partial; see [session archive](session-archive.md).
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

## An absolute, control-free `HOME` is required for integration commands

The Brain home is `$HOME/.brain`. An unset `HOME` reaches Brain as `.` and an
empty one as the empty path, so the paths it would plan were relative to the
current directory; a control character (including DEL) cannot be written into
hooks, JSON or TOML commands or desktop entries. The following commands now
refuse, before any file is read, created or changed (dry runs included), when
`HOME` or the Brain home derived from it is unset, empty, relative, not UTF-8
or contains a control character:

- `brain init --agents` (also `--dry-run`/`--yes`): refused before the
  database is created and before any client file, backup, manifest or stable
  copy is touched. Plain `brain init` without `--agents` is unchanged;
- `brain uninstall` (also `--dry-run`): refused before the manifest, backups
  or client files are read;
- `brain ui --install-shortcut` / `--remove-shortcut` (also `--dry-run`):
  refused before the running image or `user-dirs.dirs` is looked at; resolved
  menu, icon and desktop destinations containing control characters are
  refused before launcher writes;
- every `brain clients` subcommand (register, list, forget, configure,
  unconfigure).

The message is fixed and never quotes the path. Paths are not canonicalized
or followed, so an absolute `HOME` that is an alias (for example
`/home -> /var/home`), and Unicode or space-containing absolute homes, work as
before. Other commands are not covered by this check and no claim is made
that every command validates `HOME`. The check is lexical: a same-user
process that later replaces a directory on an accepted path is outside it.

**If an earlier version already ran with a bad `HOME`.** Brain may have
created `.brain`, client files or launchers relative to the directory it was
started in. Brain does not search for or delete them. Set `HOME` explicitly to
your real absolute home, then inspect only the specific paths you recognise
(for example a stray `.brain` in that directory) and keep your own client
profiles and originals; do not use wildcards or delete anything you cannot
attribute. Run `brain uninstall --dry-run` and `brain ui --remove-shortcut
--dry-run` with the correct `HOME` to see what Brain itself recorded there.
