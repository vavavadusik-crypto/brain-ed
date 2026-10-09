# brain-coord/1 network transport

Status: loopback core stage (phase 6). Date: 2026-10-09.

## What this is — and is not

`brain-coord/1` is a **custom application protocol over HTTP/1.1** (HTTPS in
a later stage). It is intentionally **not MCP Streamable HTTP** and **not MCP
Authorization (OAuth 2.1)**. The stdio MCP adapter remains the only MCP
surface and forwards only the `work_*` tools. There is no authorization
server, no RFC 9728 metadata, no RFC 8707 audience binding and no token
passthrough; credentials are local `0600` files issued by the coordinator
host's admin. Anything that speaks this wire must implement the rules below,
not MCP's.

## Endpoint and envelope

* One endpoint: `POST /v1/call`. No other path, no health route, no metadata
  route — an unauthenticated probe learns nothing.
* Request body: `{"protocol":1,"tool":"work_*","agent":<optional>,
  "arguments":{...},"request_id":<optional>}`. Unknown top-level keys,
  unknown `arguments` keys and identity keys (`machine`, `machine_id`,
  `principal`, `token_id`, `space_owner`) are refused with 400.
* The 13-tool allowlist is exactly: `work_board`, `work_show`, `work_claims`,
  `work_add`, `work_depend`, `work_claim`, `work_claim_path`, `work_renew`,
  `work_release`, `work_set_state`, `work_handoff`, `work_verify`,
  `work_whoami`. Everything else — including `memory_*`, `archive_*` and
  `sql` — answers 404 `unknown_tool`.
* Domain outcomes (busy, conflict, stale fence, not_owner) are `HTTP 200`
  documents with `result.status:"error"` and `result.error_kind`. Transport
  failures use 400/401/403/404/405/413/503 with a uniform, uninformative
  JSON body.

## Identity binding (the rule this stage added)

Identity comes from the credential, never from the request body:

* Each credential is bound at issuance to one `(machine_id, agent_id)`
  principal. Two agents on the same machine are two principals with two
  credentials; neither can act as the other.
* `agent` in a request is **optional**. When omitted, the server acts as the
  credential's bound agent. When present, it must equal the bound agent or
  the call is refused (403) — whatever the tool, whatever fence is presented.
* `machine` can never be supplied: the coordinator Actor is always built from
  the credential's `(agent_id, machine_id)`.
* `work_whoami` reports the authenticated `agent` so a caller can verify the
  binding without side effects.
* Idempotency is keyed per `(space, agent, machine)`: the same `request_id`
  under a different credential is a fresh call, never another principal's
  stored answer.
* A display label is not identity; `label` exists for humans only.
* A credential proves its principal, not a physical machine: the server
  makes no claim beyond "the holder of this secret".

## Credentials

* Format `brc1.<token_id>.<secret>`; secret is 256 random bits, only
  `blake3(secret)` is stored. Lookup is by token id with a constant-time
  hash compare; a miss hashes anyway so timing does not leak existence.
* Unknown, revoked, expired and disabled all answer the same uniform 401.
  Revocation takes effect on the next request (no cache).
* Client credential files must be `0600`, must not be symlinks, and are
  re-read each call. The token travels only in the `Authorization: Bearer`
  header — never argv, URLs, query strings, logs or error text.
* `Debug`/`Display` of token-bearing types are hand-written and redacted;
  nothing derives `Debug` over secret material. Tests assert the secret
  never appears in `{:?}` output.

## Transport rules (loopback stage)

* Binds `127.0.0.1` only. A loopback `Host` header is required; `Origin` is
  refused; a token in the query string is refused before parsing.
* Remote binding without TLS is refused by design. The TLS listener
  (below) also binds `127.0.0.1` only; a LAN listener is a separate,
  explicit host-admin deployment gate that no command offers yet.
* Bounds: 8 KiB header block, 64 KiB body, 64 concurrent connections, 5 s
  read timeout, chunked transfer refused. Oversize answers 413/431 or the
  connection is closed — never a fabricated success.
* Check order per request: route → Host → Origin → method → token parse →
  body parse → one state lock → authenticate → authorize (agent binding,
  space allowlist, role) → schema → coordinator dispatch.
* The client is fail-closed: connection errors are `Unreachable`, never
  retried against a different endpoint, and there is **no local fallback
  coordinator**.

## Storage

* `auth.sqlite` (application_id `0x42524E41` "BRNA", schema 2) holds
  principals and credential hashes; the coordinator's `coord.sqlite`
  (schema 1) is untouched. Foreign, newer or partial files are refused as
  `incompatible`; nothing is migrated silently. Schema 1→2 (agent binding)
  revokes unbound credentials rather than guessing an identity for them —
  schema 1 was never released.
* Directory `0700`, database and `-wal`/`-shm` `0600`.

## TLS and the `brain net` CLI (loopback stage)

* rustls (ring), safe default versions (TLS 1.2/1.3). The client trusts the
  `--ca` file ONLY — no system roots, no trust-all, no plaintext fallback —
  and checks the certificate SAN against `--server-name`.
* The client **never resolves names**. `--dial` is a numeric loopback
  `IP:PORT` (hostnames, LAN and unspecified addresses are `invalid_config`);
  the certificate name is a separate argument. One absolute deadline
  (`--deadline-ms`, 50..=5000) bounds connect + handshake + request +
  response; every socket operation gets only the time left.
* Server key and client credential come from an owner-only file
  (`O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC`, regular, this uid, not group/world
  accessible) or an inherited descriptor `>= 3` (`--key-fd`,
  `--credential-fd`), reopened via `/proc/self/fd/N` and held to the same
  checks: a pipe, FIFO, socket or loose file is refused without blocking.
  There is no flag that takes a token; tokens never reach argv/stdout.

```
brain net serve --state-dir DIR --cert CRT (--key KEY | --key-fd N) [--port P]
brain net enroll --state-dir DIR --machine M --agent-id A --role worker|reader \
    --grant-space S... --credential-out NEWFILE [--ttl-secs N]
brain net revoke --state-dir DIR (--token-id ID | --machine M)
brain net principals --state-dir DIR
brain net status --dial 127.0.0.1:P --server-name localhost --ca CA (--credential F | --credential-fd N)
brain [--space S] net call <same connection flags> whoami|board|show|claims|add|depend|
    claim|claim-path|renew|release|state|handoff|verify ...
```

* `serve` prints one JSON line with the real bound address, serves until
  stdin closes (owned stop: refuse dispatch, cancel sockets, join threads),
  then prints `stopped`. Under a supervisor stdin must stay open.
* `enroll`/`revoke`/`principals` are trusted local administration of the
  state directory; they never use the network. Revocation takes effect on
  the next request, before any idempotent replay could be answered.
* `call` exposes typed subcommands only, each mapped to one allowlisted
  tool with a fixed schema; there is no free-form tool name. The global
  `--agent` is not sent: the credential binds agent and machine.
* Exit codes: 0 ok, 2 invalid_config, 3 domain_error (busy, conflict,
  stale fence), 4 refused (401/403/400/404) or admin not_found,
  5 tls_failure, 6 unreachable (nothing sent), 7 unknown_outcome.
* Retries are the caller's: `add`, `claim`, `claim-path`, `state`, `handoff`
  always carry a request id (generated and printed when not given), so
  `unknown_outcome` says to rerun with the same `--request-id`. `renew`,
  `release`, `depend` carry none; their `unknown_outcome` says to query
  (`verify`/`show`) first. Nothing retries automatically.

## Not in this stage

LAN binding, MCP forwarding to a remote coordinator, memory/archive
sharing (a separate sharing core; not routed through brain-coord/1),
credential rotation UI, hooks, benchmarks.

## Shared memory reads (phase 25)

`share_pull` and `share_query` are the 14th and 15th allowlisted tools.
They are reads, need a host-local memory grant (`brain net share grant`)
on top of the credential's namespace, and the host must have enabled the
space. Details and limits: `docs/shared-memory.md` ("Wire and CLI").
