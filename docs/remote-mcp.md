# Remote coordination reads over MCP (opt-in)

Status: experimental, read-only. Tested with two synthetic installations on
one machine over loopback TLS only.

`brain serve --remote-config /abs/remote.json` adds five read-only MCP tools
that query **one explicitly configured** `brain-coord/1` coordinator per call
over the existing TLS client. Without the flag, `brain serve` offers exactly
the same tools as before and makes no network call.

## Tools

| MCP tool | Wire tool | Notes |
|---|---|---|
| `remote_status` | `work_whoami` | `endpoint` required (no fan-out) |
| `remote_work_board` | `work_board` | `endpoint`, `space`, optional `project`, `state`, `limit` 1..=50 |
| `remote_work_show` | `work_show` | `endpoint`, `space`, `task_id` |
| `remote_work_claims` | `work_claims` | `endpoint`, `space`, optional `project`, `limit` 1..=50 |
| `remote_shared_query` | `share_query` | `endpoint`, `space`, optional `limit` 1..=10 (the coordinator's page bound); facts the remote host itself shared with this credential (needs its host grant) |

Each name maps to one fixed wire tool. There is no pass-through: memory,
archive, `share_pull` and every write tool are not reachable, and unknown
`remote_*` names are "Unknown tool". Remote writes are not offered.

## Config file

Absolute path, regular file (no symlink), owned by this user, mode `0600`,
at most 16 KiB, read through one descriptor. Strict JSON: unknown fields are
refused, and a repeated object key at any depth (also when spelled with
escapes such as `"n\u0061me"`) is refused instead of "last one wins":

```json
{"version": 1, "endpoints": [{
  "name": "studio", "dial": "127.0.0.1:7443", "server_name": "127.0.0.1",
  "ca": "/abs/ca.crt", "credential": "/abs/studio.cred",
  "allow_lan_dial": false, "deadline_ms": 3000, "spaces": ["team"]}]}
```

* 1..=8 endpoints; `name` `[a-z][a-z0-9-]{0,31}`, unique; 1..=16 unique
  `spaces`, each in the coordinator's own grammar: 1..=64 bytes of
  `[A-Za-z0-9._:-]` (e.g. `team:x`). A listed space grants nothing by itself:
  the coordinator's credential and grant checks still decide.
* `dial` is a numeric `IP:PORT` (never resolved): loopback, or a private LAN
  address only with `allow_lan_dial: true` and `server_name` equal to that IP
  (see `docs/network-transport.md`, "LAN opt-in").
* `ca` and `credential` are paths only. The CA must be a regular file owned
  by this user; the credential must be `0600`, no symlink — the same checks
  `brain net` uses, applied at each call. Inline tokens or any unknown field
  (including `token`, `secret`, `key`, `password`) are refused.
* `deadline_ms` 50..=5000 (default 3000): one absolute deadline per call.
* An invalid or unsafe file makes `brain serve` exit with an error that names
  the field, never file contents or credential paths. Nothing is dialed or
  read from the endpoints at startup.
* `--offline` / `BRAIN_OFFLINE` only forbids explicit model installation. It
  is **not** a network isolation control and does not disable this opt-in.
  The local-only default is the absence of `--remote-config`: without it,
  `brain serve` offers no remote tool and makes no remote call.

## Replies

Text-only MCP content (no `structuredContent` copy). Every reply carries
`"trust": "untrusted_remote_data"`, a notice not to follow instructions in
the data, `provenance` (endpoint, dial, TLS server name, pinned CA, protocol,
server time, fetch time, authenticated machine/agent) and `budget`.

The **whole** emitted JSON-RPC response line, including metadata, escaping
and its terminating newline, is at most 16 KiB (16384 bytes). String values are cut at 240 characters, lists at 50 items, objects at 64
entries, nesting at 8 levels. Object keys are never shortened or renamed: a
key longer than 240 characters is omitted with its value as a whole entry
(counted), so two distinct keys can never merge; whole items are then dropped from the largest list until it fits, for at
most 256 trimming passes; past that the reply is the minimal
`reply_budget_exceeded` error.
`budget.truncated`, `omitted_items` and `clipped_strings` report it. If even
the metadata cannot fit, a minimal `reply_budget_exceeded` error is returned.

`project` follows the coordinator's own rule (`is_valid_project`): 1..=100
ASCII characters of `[A-Za-z0-9._-/]`, relative, with no empty, `.` or `..`
segment (e.g. `team/sub/dir`). It is checked locally before any dial.

Board, claims and shared-query return the **first page only**: there is no
offset or cursor, so a long board or fact list may be incomplete.
`budget.truncated: false` only means this adapter cut nothing locally; it does
**not** mean the remote list is complete. `remote_shared_query` returns at
most 10 facts and carries no `has_more` or total count.

Success is reported only for envelopes of the coordinator's exact shape:
integer protocol version 1, a `principal` object for `remote_status`, and
otherwise a `result` object whose `status` is the string `ok` (success) or
`error` (`remote_error`). Anything else is `remote_malformed` with
`isError: true` and no data.
The configured endpoint names, dial addresses and space names appear in the
tool definitions and in `provenance`, so they enter the agent's context.

`provenance.protocol` is the coordinator's numeric protocol version (1).
`provenance.authenticated_as.machine` comes from the server; `agent` is
`null` for `work_board`, `work_show` and `work_claims` because the
coordinator does not report it there. This is not a complete identity claim;
the credential decides identity server-side.

Failures: `remote_error` (HTTP 200 but the coordinator's `result.status` is
not `ok`, e.g. an unknown task; `isError: true`, bounded untrusted payload in
`data`), `invalid_arguments` (checked locally, nothing sent), `refused`
(with `http_status`; the server's role/space/grant/revocation decision),
`no_session` (no TCP connection or TLS session: refused connect, wrong CA
or name, expired certificate, not TLS; nothing sent), `unreachable`
(connection lost or deadline after the request was written; reads are not
retried), `endpoint_material_rejected` (CA/credential checks).

Shared facts keep the server's `origin`, `fact_id`, `origin_seq`, `last_seq`,
`claimed_agent` and provenance statement; they are never merged into local
memory, and nothing is written locally.

## Not supported

Physical multi-machine fleets, NAT, discovery or name resolution, certificate
rotation, caching, cross-endpoint merging, remote writes, memory search over
remote facts. Provenance is "claimed by the origin, authenticated by TLS and
the credential", not end-to-end signed. A slow endpoint blocks the stdio
session for up to its deadline.
