# ADR 0001 — Work coordination foundation

Status: accepted for phase 4 (local foundation). Date: 2026-10-08.

## Problem

Codex, Claude Code and Hermes share memory but not *work*. Two agents can start
the same task or edit the same files. We need: who works on what, on which
machine, until when — and a way to refuse a stale worker.

## Options considered

| | A. Central authoritative SQLite coordinator (chosen) | B. P2P / CRDT replicated board | C. Embed in `brain.sqlite` |
|---|---|---|---|
| Correctness of exclusive claims | Linearizable: one `BEGIN IMMEDIATE` writer at a time | CRDTs merge; they cannot give mutual exclusion without a consensus layer | Same as A |
| Multi-machine path | One coordinator process + thin authenticated adapters (later) | Native, but needs conflict UI and still no exclusivity | Same as A |
| Blast radius / rollback | Separate file; old binaries never open it | New sync stack | New migration on the memory DB; rollback needs down-migration |
| Effort now | Small | Large | Small, but couples schemas |

A CRDT is the wrong tool for a lock: mutual exclusion needs one authority.
Option C was rejected because the memory DB is hot, large and versioned; a
separate `coordination/coord.sqlite` keeps rollback trivial (delete or ignore
the file) and lets the coordinator later move behind a service unchanged.

## Decision

* `brain-core::coordination` owns `$BRAIN_HOME/coordination/coord.sqlite`
  (dir 0700, file 0600, WAL, `application_id` = `0x42524E43` "BRNC",
  `user_version` = 1). A file with another application id, a newer version or
  missing tables is refused as `incompatible`; nothing is migrated in place.
* Every write is one `BEGIN IMMEDIATE` transaction. Time is read **inside**
  the transaction from the coordinator's clock, clamped to a stored
  high-water mark so it never runs backwards. Clients send durations
  (`ttl_secs`, integer 10..=86400), never timestamps.
* Owner identity = `(agent, machine_id)`. `machine_id` is a random installation
  id stored once in `$BRAIN_HOME/machine-id` (`m-` + 32 hex). It is never
  derived from a model name or hostname. `BRAIN_MACHINE_ID` may override it
  (tests / simulations) and the answer says so.
* Tasks: `queued | active | paused | completed | failed`, scoped by
  `(space, project)`. Dependencies only inside one space; cycles refused;
  a task whose dependencies are not all `completed` cannot be claimed
  (`blocked`).
* Claims (task or path) carry a lease and a **fencing token** from one
  monotonic per-database counter (Kleppmann). Every mutating call on a claim
  must present `(claim_id, fence)`; the store rejects anything that is not the
  current live claim: `stale_fence` after re-claim/handoff, `lease_expired`
  after expiry, `not_owner` for another identity.
* Expired leases are taken over by the next claimant with a strictly greater
  fence. Renewal after expiry is refused — the owner must re-claim.
* Path claims are exclusive, scoped by `(space, project)`. Identity rule:
  lexical normalisation only (`/`-separated, `.` removed, `..` resolved,
  duplicate and trailing slashes removed; no symlink resolution, case
  sensitive, no Unicode normalisation). Two paths overlap when equal or when
  one is an ancestor of the other on a component boundary (`src` overlaps
  `src/a.rs`, not `src2`). Same owner may hold overlapping claims. This is
  advisory coordination, not filesystem locking.
* Handoff closes the current claim and opens one for the target owner with a
  new fence in the same transaction; the event records both owners.
* Idempotency: optional `request_id` per `(space, agent, machine)`; a retry with
  the same id and same request returns the stored answer, a different request
  is `idempotency_conflict`. Kept 24 h.
* Reads are paged (`limit` ≤ 100) and bounded; context packets carry at most
  the last 20 events and 8 KiB of text. Task text written by agents is labelled
  `agent_supplied`; ownership/lease data is `coordinator_state`. The private
  conversation archive is never read by the coordinator.

## Network transport — brain-coord/1 (loopback core built; TLS remote is next)

SQLite WAL "does not work over a network filesystem" (sqlite.org/wal.html) —
the DB is never shared over NFS/SMB. The transport now exists as a **custom
HTTP/1.1 application protocol, `brain-coord/1`** (see
`docs/network-transport.md`). It is deliberately **not** MCP Streamable HTTP
and **not** MCP Authorization/OAuth 2.1: the stdio MCP adapter stays the only
MCP surface, and credentials are local `0600` `brc1.` files issued by the
coordinator host's admin — no authorization server, no RFC 9728/8707, no token
passthrough. Thin adapters forward `work_*` calls; the server binds each
credential to one `(machine_id, agent_id)` at issuance, derives the actor
from the credential (a request `agent` must match it or be omitted), and
refuses any `machine_id` from the wire. This stage binds loopback
(`127.0.0.1`, loopback Host, no Origin); TLS plus a remote listener is the
remaining goal, using rustls already present in the lockfile. Optional
heartbeat/capability registry later.

## Phase 4 plan

1. Core module + unit tests (race with 10 threads, fencing, expiry, overlap,
   scope, handoff, idempotency, schema checks).
2. CLI `brain work …`, MCP `work_*` tools, read-only `/api/work/board` + settings
   panel.
3. Process test: 10 `brain` processes with distinct machine ids race one claim.
4. Research note, GLM review of the diff, fixes, release build, promotion script.
