# Shared memory core (opt-in facts, links, provenance)

Status: core slice (phase 17) plus the TLS wire slice (phase 25,
2026-10-09, branch `codex/shared-product-transport`). Shared facts now
travel between installations over `brain-coord/1` loopback TLS through two
typed read tools and the `brain net share` CLI (see "Wire and CLI" below).
Not wired: MCP tools, retrieval/search, automatic capture, LAN binding.

## What it is

`brain-core/src/sharing.rs` is a self-contained store for opt-in shared
memory: chosen facts (and links) with provenance, exchanged between
installations. It is **not** a shared task board. Every installation keeps one
`share.sqlite` (application id `BRNS`, schema version 1) whose path is
parameterized. Sharing is default deny:

* The host explicitly enables whole spaces (`enable_space`).
* A caller must present an authenticated `Principal` whose credential carries
  an explicit `memory:read:<space>` grant.
* Both are checked **before** every query and before replay
  (`authorize_read`), never after. Credentials issued before sharing existed
  carry no grants and get nothing automatically.
* Authorization consults only the host-built `Principal`. No payload field,
  boolean, claimed origin/agent or project text can raise authority; payload
  keys that name host context (`trusted_peer`, `host_context`, `authority`,
  `grant`, `credential`, `token`) are refused outright.

## The event stream

One append-only sequence per `(origin, space)`. `put`, `retract` and
`supersede` are all events on that one stream, so a peer pulling a single
space sees a gap-free sequence. Materialized facts (`facts` table) reference
the original `put` event via `origin_seq`; supersession replaces the payload
but keeps every prior payload in the event history. Nothing is ever updated
in place in `events`, and nothing is deleted.

* Event identity is `blake3` over a canonical encoding (`brain-share-event/1`)
  that includes the canonical JSON payload (keys sorted recursively).
* Applying the same event bytes again is idempotent (even via a different
  peer); the same `(origin, space, seq)` with different bytes is a
  `share_conflict` and rolls back the **whole batch** — the cursor never
  advances on a partial apply.
* Gaps, out-of-order batches and a cursor that does not match the stored
  value are rejected (`gap`, `cursor_mismatch`).
* Paging is over the event stream: `page` returns up to `MAX_PAGE = 100`
  events after a sequence number; the pull cursor is one row per
  `(peer, space, origin)`. There are no separate facts/events cursors.
* Every served event is re-digested and compared to the stored digest, so a
  tampered row never travels.

## Provenance: claimed vs trusted

Each event records `claimed_agent` — what the remote side claims, treated as
data, never consulted by authorization. The applied row separately records
`peer_id`/`applied_at` from the trusted host context at apply time. The
`map_packet` adapter (`shared_snippets`) keeps both apart in each item's
`shared` object (`origin`, `fact_id`, `origin_seq`, `claimed_agent` — the
origin's claims — and `received_from_peer`, recorded by this host), with
`trust: shared_claim_untrusted`. Kinds are never relabelled: a shared fact is
`kind: fact`, a link `kind: link` (its refs stay in the text), both in
`MEMORIES`.

## Retrieval into map packets: host-configured only

A packet reads shared facts only when the host process was started with
`--share-state-dir <ABSOLUTE DIR>` (e.g. `brain --share-state-dir /x serve`)
and the request carries `memory_query`. Unset, nothing shared is read; a
missing store is read as empty and never created; a relative path is
refused. No MCP argument can name or change the store (unknown arguments are
refused). The host reads its own store through `SharedReader::HostLocal`,
which checks the space is enabled on this host and nothing else; it is not a
network principal and no `Authenticated` is built for it. Remote readers go
through `SharedReader::Remote(&Principal)` (space enabled ∩ credential grant).
Scope comes from the curated payload (`project`, or `scope: "space"`) and is
applied before the per-packet limit (10) so other projects cannot crowd one
out; retracted facts are not returned. Local graph memory items now carry
`provenance: graph_memory/agent:<stored agent_id>` — the writer's declared
client label, not proof of which model ran.

## Publish is explicit only

`publish` takes a fully curated `SharedFact` payload. There is no API here
that reads the private memory graph or the conversation archive; anything
shared was chosen one fact at a time. Bounds: payload ≤ 4096 canonical bytes,
claim ≤ 600 bytes, ids/spaces `[A-Za-z0-9._-]{1..64}` (fact ids 128), seq
≤ 2^53, page 1..=100, batch ≤ 100 events and ≤ 256 KiB.

## Known limitation (by design)

Revocation cannot erase data already exported to peers. A retraction
appends to the local stream and reaches a peer only when that peer pulls;
a peer that never pulls keeps its copy, and a revoked credential only stops
future access. Provenance is recorded, not proven: a remote origin id is an
assertion by the peer we pulled from.

## Store hygiene

Directory `0700`, database and WAL/SHM sidecars `0600`. Read-only openings
never create or migrate; a missing file is `not_found`, a foreign or
wrong-version file is `incompatible`. Nothing is migrated silently.

## Wire and CLI (phase 25)

* **Authority.** `sharing::Principal` has private fields; its only public
  constructor is `Principal::from_authenticated(&Authenticated)`, and only
  `AuthStore::authenticate` can build an `Authenticated` (private grant
  field). Grants live in `auth.sqlite` schema v3 (`memory_grants`), written
  only by host-local `brain net share grant|ungrant`. A grant must name one
  explicit space inside the principal's namespace (`*` is refused); a
  narrowed namespace silently disables grants outside it. v2/v1 auth stores
  migrate with **no** grants: old tokens stay work tokens and read nothing.
  Grants and revocation are read on every request before any replay.
* **Wire tools** (read-only, allowed for `reader` and `worker`):
  `share_pull {space, after_seq, limit<=10}` returns one page of the HOST'S
  OWN origin stream (relayed events are never re-served) with per-event
  digests, `next_seq`, `has_more`; `share_query {space, limit<=10}` returns
  the host's own live facts. Unknown fields (`grant`, `machine`, `origin`,
  `trusted_peer`, `authority` …) are 400; denials (no grant, space not
  enabled, wrong namespace) are one uniform 403; revoked/disabled
  credentials 401. No publish/apply/grant tool exists on the wire.
* **Pull.** `brain net share pull --state-dir --space --peer-id --origin
  …TLS args…` pins the expected origin, parses every event strictly
  (`ShareEvent::from_wire`, digest recomputed), and applies each page with
  `apply_batch` (atomic with the cursor). The receiving host must have
  enabled the space locally; the remote reader credential is not local
  authority. A conflicting page rolls back entirely and the cursor stays.
* **Event identity** now includes `claimed_agent`; an event whose fact space
  differs from its stream space is invalid.
* **Store hygiene fixes.** The store path and `-wal`/`-shm` must be regular,
  own-uid, non-symlink files (`SQLITE_OPEN_NOFOLLOW` as well); the file is
  created `0600` *before* SQLite opens it, and no extra descriptor is ever
  opened on a live SQLite file (closing one drops the process's POSIX locks —
  this bug made a running server keep reading a stale `host_spaces` snapshot
  in the first process run). Existing parent directories are never chmodded;
  only directories created here are `0700`.

Process evidence: `brain-cli/tests/share_tls_process.rs` (two synthetic
installations, real loopback TLS, ten simultaneous pulling processes on one
machine). Not ten computers, not LAN, not MCP.

Root verification: `cargo test -p brain-core --lib sharing:: -- --test-threads=1`
passed all 22 tests on 2026-10-09. The independent run found and fixed
`enable_space` failing to re-enable an existing disabled row; the regression
checks disable and re-enable. These are core tests, not TLS or physical-machine
deployment evidence. The adapter is not wired into retrieval yet.
