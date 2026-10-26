# ADR 0002 — Team facts

Russian user guide: [Командный режим](../team-mode.ru.md).
The Russian owner draft is excluded from the public source export.

Status: **a draft for the owner, not adopted and not implemented**. Date:
2026-10-26. Verified base: `da88d19`, worktree `e7-team-adr`. The number 0002
is the next one after the existing [ADR 0001](../adr-0001-coordination.md);
E7.1 is the stage number.

Basis: the owner's plan of 2026-10-24
the owner's internal E7 plan (2026-10-24, not published),
read in full, including "Owner decisions" and the 2026-10-25 additions.
The external path here is a link to the development basis, not a field of a
published slice.

## Task and the solution proposed to the owner

The lead gets the state of the selected machines: tasks, plans, errors,
decisions, links, Git changes, agents/models and brief session summaries, with
the machine, session and time attached to every result. Local memory stays
local. What is published is a separate verified projection of facts, not a copy
of databases or the archive. Sources and model-extracted assertions remain
untrusted data.

The owner's maximum-visibility decision means: after a machine is explicitly
enabled and projects are chosen, all listed classes of safe facts are offered.
Before enabling, without a manifest or grant, or with an undefined project,
publication and delivery are forbidden. Every class/field can be switched off;
an unknown field is closed. This is not enterprise/fleet readiness and not
proof of security.

## What actually exists in the verified code

| Basis | Verified functions/structures | Limitation |
|---|---|---|
| Explicit publication of one fact | [net/share.rs](../../brain-cli/src/net/share.rs): `run`, `curated`, `publish`/`supersede`/`retract` via `ShareStore` | `--payload` is an arbitrary JSON object; there is no automatic work/distill/git export |
| Isolated exchange | [sharing.rs](../../brain-core/src/sharing.rs): `SharedFact`, `ShareEvent`, `authorize_read`, `page_own`, `own_live_facts`, `apply_batch` | Two kinds, `fact`/`link`; `live`/`retracted` is the publication state, not the error life cycle |
| TLS and administration | [net.rs](../../brain-cli/src/net.rs): `serve`, `enroll`, `revoke`, `principals`, `client`; [server.rs](../../brain-core/src/network/server.rs): `dispatch_share` | A grant permits reading a space; there is no field policy or project-level rights here |
| Fact extraction | [distill.rs](../../brain-cli/src/distill.rs): `parse_fact`, `write_fact`, `write_links`, `allowed_transition` | Evidence and transition validation does not confirm the fact's truth or the safety of exporting it |
| Work board | [work.rs](../../brain-cli/src/work.rs): `execute`, `mcp`; [coordination.rs](../../brain-core/src/coordination.rs): `Coordinator::{board,show,claims}`, `SCHEMA` | Cooperative leases, not file watching; title/detail/note are untrusted text |

Details of the existing wire contract: [shared-memory.md](../shared-memory.md),
[network-transport.md](../network-transport.md). Sources take precedence when
older descriptions diverge. The sections below define **proposed E7
requirements** unless "now" is stated explicitly.

## Entities, fields and provenance

All records share a common envelope. The new field names below are a schema
draft, not an existing API. An unknown optional value is `null` with a reason;
unknown mandatory identity/project/time/source means the record is refused.

| Envelope field | Source now and projection rule |
|---|---|
| `schema` | New constant `brain-team-fact/1`; the exact supported value is checked |
| `entity_id`, `entity_type`, `revision` | Source ID + type + origin in the local mapping; work ID from `tasks.id`, distill from `fact_id()`, Git OID. A new projection revision number; the distill FNV-ID is not cryptographic proof |
| `origin`, `machine_id`, `machine_label` | `ShareStore::install_id()`; local identity via `work::actor`/`whoami`; `network::auth::Principal` registration is separate. The origin↔machine binding is fixed by the administrator, no coincidence is assumed; a label like «7» is display only |
| `space`, `project_id` | An explicitly allowed space; work `tasks.project`; distill `metadata.project` from the agreed evidence cwd. A new local project_id↔root registry is needed. The absolute cwd never goes out; the archive by itself is not a project ACL |
| `session_ref`, `agent_ref`, `model_ref` | Per source below; local IDs are translated into opaque export references, without paths or profiles. `null` is allowed for a non-session record with an explicit `not_applicable` |
| `source_kind`, `source_ref`, `trust` | A new typed locator to local evidence, not an archive URL and not its content; `coordinator_state`, `git_object` or `untrusted_model_extraction`. The reference grants no remote right to read the primary source |
| `valid_from`, `valid_to`, `recorded_at`, `superseded_at` | Two time axes, rules below; separately `published_at`, `received_at`, the source watermark and `coverage` (`bounded`/`unknown`) |

The content schema of each entity is closed; the text fields below are only
separately allowed, edited annotations, not a direct copy of a line.

| Entity and published fields beyond the envelope | Exact source in the current code / missing part |
|---|---|
| Task: `task_ref`, `title_summary`, `state`, `created_at`, `updated_at`, `owner_agent`, `owner_machine`, `lease_expires_at`, `dependency_refs` | `coordination::SCHEMA`: `tasks`, `claims`, `task_deps`; `board`, `show`, `claims`. The effective state from `EFFECTIVE_TASKS` is published: active without a live lease reads as queued. Do not export the whole `show` JSON with note/data/detail |
| Plan: `fact_ref`, `what_summary`, `why_summary`, `status`, `task_refs` | `distill::KINDS` includes `plan`; `write_fact` writes the `kind/what/why/status/project` metadata. OTHER statuses: proposed/active/completed/abandoned/confirmed. Links to work tasks are not created automatically; only an explicit match, otherwise an empty list |
| Error: `fact_ref`, `what_summary`, `why_summary`, `status`, `reason_summary`, `evidence_refs`, `transition_refs` | `ERROR_STATUSES`, `parse_fact`, `write_fact`; evidence/evidence_by_session/when_min/when_max in the metadata. `reason_summary` is derived from the allowed why annotation; there is no separate mandatory reason field in the code. For a team `wont_fix` a non-empty reason is mandatory |
| Decision: `fact_ref`, `what_summary`, `why_summary`, `status`, `evidence_refs`, `error_refs` | `DECISION_STATUSES`, `write_fact`: proposed/adopted/worked/failed/reverted. `failed` does not create an error automatically. `error_refs` — only a confirmed link that actually exists; mark a missing link unknown |
| Link: `from_ref`, `to_ref`, `relation`, `evidence_refs` | `distill::edge`, `write_links`: caused_by/fixed_by/superseded_by/became; `write_fact` also has_session, fixed_by/became to the session node. Do not present a session as the fixing commit. Work dependencies from `task_deps` are projected as a separate depends_on type. Both ends must be allowed in the same published project |
| Commit/merge: `oid`, `parent_oids`, `committed_at`, `is_merge`, `subject_summary`, `task_refs` | A new local Git adapter: the commit object in an explicitly chosen repository, OID/parents/committer timestamp; is_merge = parent count >1. `share.rs`, `distill.rs` and the work board have no such collector. Do not infer from a commit the publication date, test success or the fact of an accepted PR; do not guess a squash as a merge |
| Agent/model participation: `agent_ref`, `client_kind`, `model_ref`, `model_evidence`, `started_at`, `ended_at`, `task_refs` | Work `Actor`, tasks.created_by_agent/machine, claims.agent/machine and times; graph `Attribution.agent_id/session_id` is the writer, not proof of the model. Archive `sources.client` is the client. `DistillEntry` and archive schema 3 have no structured model; `extract_chunk` calls the LLM but does not save the winning model in the fact metadata. Until a separate confirmed source exists, model_ref=null, model_evidence=unknown; a config/agent name does not prove a run |
| Session summary: `session_ref`, `client_kind`, `first_observed_at`, `last_observed_at`, `summary`, `fact_refs`, `task_refs`, `coverage` | [archive.rs](../../brain-cli/src/archive.rs): `DistillEntry`, `distill_sessions`, `distill_entries`; [session_archive.py](../../tools/session_archive.py): `SCHEMA/SCHEMA_3`, entries.session_id/ts/imported_at/cwd and sources.client. There is no ready-made safe summary here; a new summarizer of allowed facts only. `distill::write_fact` creates a Session with a stub string, not a summary. The min/max of visible ts do not prove the start/end of a full session |

`dead_end` and `finding` already exist in `KINDS`; we propose including them as
separate fact subtypes with the same rules, not auto-converting them into
errors/decisions.

### Life cycle and the two time axes

The owner-facing picture: suspected → confirmed → fixed / wont_fix (why) /
not_a_bug / became_feature. `confirmed` is a confirmation in the source, not
the lead's independent expertise. "Not yet fixed" is confirmed with an open
outcome, not a synonym of `wont_fix`.

Today `allowed_transition("error", …)` allows suspected→confirmed/fixed/not_a_bug
and confirmed→fixed/wont_fix/not_a_bug/became_feature. For a decision:
proposed→adopted/reverted; adopted→worked/failed/reverted;
worked→failed/reverted; failed→reverted. Publication preserves these
transitions and does not invent intermediate states. Reopening a terminal
error is not supported by the current state machine. `parse_fact` checks that
evidence comes from the same chunk/session and has an allowed role; a status
change in `write_fact` requires new evidence and an exact `status_evidence`
declaration. This is not a universal validator of any natural language and not
a guarantee of the initial status's truth.

For distill we take node versions from
[model.rs](../../brain-core/src/model.rs) (`Attribution`, `AsOf`, `Node`) and
[storage/sqlite.rs](../../brain-core/src/storage/sqlite.rs) (`replace_node`):
valid_from/valid_to — when the assertion holds, recorded_at/superseded_at —
when this version became known to the store. `write_fact` sets the valid_from
of a new fact to when_min, of a transition — to when_max, of a replacement —
with a new recorded_at; that is the time of the evidence, not a guaranteed date
of the actual fix. when_min/when_max are stored as evidence bounds, not as the
error's life interval.

Work stores mutable state and `events.at` events, but not a ready bi-temporal
table. For it, a new projector starts the history at its first observation;
created_at/updated_at/events.at are the source times, recorded_at is the local
observation time, superseded_at is the next version. Do not claim historical
completeness. For Git, valid_from = committer timestamp (a Git assertion),
recorded_at = first observation; the client's clock may be wrong. For
summaries, valid_from is the start of the observed window and valid_to is its
end; for agent participation, unknown intervals are not guessed.

A query specifies both as_of coordinates (`valid_at`, `recorded_at`); the
intervals are half-open. Correction history is added as versions; supersede of
a publication by itself does not implement as-of queries. `ShareEvent.seq` and
`FactRow.applied_at` do not replace bi-temporality: the history must live in
the new published contract and a local index. Mark history before the first
observation unknown; do not reconstruct it from the current task state.

## Prohibitions and the fail-closed boundary

In ordinary Team facts the following are **never published**:

- Raw user/assistant messages, reasoning, `status_evidence` quotes, prompts,
  tool_call/tool_result/tool_error, stdout/stderr, raw model answers and logs.
- Secrets, credentials, tokens, keys, env, profiles and personal archives;
  Git author/email, remote URL and diff are not part of the allowed schema.
- Absolute paths, cwd, sources.path/locator, paths outside the chosen project,
  including via symlink/`..`, URLs with credentials and embedded paths in
  annotations. Original archive session/profile identifiers are replaced with
  opaque refs.

The current `SharedFact::validate` bounds sizes and reserved keys;
`reject_reserved_payload_keys` checks nested host-context keys. It is **not**
a DLP filter. `curated` does not inspect string content.
`distill::parse_fact` masks what/why through `redact::mask_secrets`, but
pattern masking does not prove the absence of an arbitrary secret or quote.
The current share must not be declared a safe automatic export of the archive.

E7.2 requires a separate boundary before writing to the share store and before
delivery:

1. An explicitly enabled local manifest; the exact project_id and root from
   the registry, the exact space and an allowed recipient. Uncertainty →
   refusal.
2. A typed projection over a field allowlist, without serializing the original
   metadata/body/note/data wholesale and without remote reading of archive
   evidence.
3. Numbers, enums and IDs pass a strict schema; annotations are prepared
   separately and approved by the local owner before export. Automatic text
   generation yields only a candidate, not permission. Text without approval
   means the field is closed; a mandatory closed field → the record is
   refused. A secrets/paths detector may additionally refuse but does not
   replace approval.
4. Paths, when needed, are only verified project-relative refs: canonical
   containment for existing objects, refusal on an unavailable object/symlink
   escape; no replacing an outside path with a single basename.
5. Re-check of the prepared bytes against the current policy version,
   including claim, references, label and other string envelope fields, not
   only the payload. For team facts the claim is proposed to be always null;
   an unsupported schema, a limit overrun, an unavailable validator or an
   invalid record → do not publish it, store only the reason code locally.
   The slice is marked incomplete; a remote response must not disguise a
   refusal as no work.
6. A dedicated E7 space serves only validated records of this schema; manual
   `share publish` must not bypass the check. Old arbitrary facts do not
   become team facts automatically; loading/delivery has no fallback to raw.

These are requirements for implementation and future negative tests, not an
active guarantee. Minimal E7.2 acceptance: synthetic messages, tool output,
secrets, paths/symlinks, forged project/authority and unknown fields do not
get out — not in payload, claim, identity, errors, summaries, nor the journal.
Secret values are not printed in the report: only file:line and type. Screen
and arbitrary-file monitoring is not introduced by this decision.

## A separate enterprise raw-text mode

Switched off, not implemented in E7.1. The owner's decision allows considering
it separately; it does not extend team-facts/1. It needs a separate ADR and
explicit enabling by an administrator and on the machine, defined
purposes/consents/legal basis, exact sources/projects/recipients/fields/
retention, separate spaces and permissions, encryption and key management,
access audit, retention/deletion and an incident procedure. Secrets and
outside paths remain forbidden. The current archive has no project ACL, so
porting an existing memory grant to raw is forbidden. No automatic fallback
when distillation/search fails.

## Local manifest and request journal

New E7.2/E7.4 entities; they do not exist today. The manifest on each machine
stores: the version/revision and time of local approval, enabled, the
origin↔machine binding, project_id↔local roots (roots are visible only
locally), spaces, classes/fields, recipients principal(machine,agent), the
schedule, the watermark/last successful publication, refusals and coverage, the
freshness term, and the annotation and integrity policy. The settings show
"what, to whom, when" and a switch for every item. Switching off stops new
delivery; a changed policy requires a new projection and retracting the extra
facts. Until the check/rebuild completes — deny.

The source machine needs a journal of **every** lead request, including deny,
errors and cancellations: request_id, receipt/completion time, authenticated
machine/agent/token_id (not the token), projects/spaces, operation, as_of, the
policy/slice revision, the count and refs of issued records, the outcome,
latency, the refusal reason. Question/answer text and tool output are not
copied into the journal; by default only safe structured filters. The identity
of the human behind an assistant requires a separate binding and is not derived
from a label. Unauthenticated refusals have an unknown principal. The audit
must successfully accept the record before data is delivered; with the journal
unavailable, delivery is refused. Retention of and access to the journal itself
are defined by the owner's local policy.

The current `coordination::requests` is a write idempotency cache, `events` is
a journal of work operations. They are not a journal of all share/read
requests. The `net/share.rs::status` output shows enabled spaces/counts/grants
but does not replace the full manifest or an interface for configuring it.

## Slice format, integrity and transport

`brain-team-slice/1` is proposed: manifest_revision, origin, space, project_id,
snapshot_id, generated_at, source_watermarks, coverage, seq_range,
entity_refs/digests, integrity and signature. `brain-team-fact/1` entities are
transferred as separate `SharedFact`s (kind=fact/link), not one huge payload.
The slice manifest pins the complete set/generation; applying a slice for
search is atomic after all pages are verified; until then only the previous
still-allowed generation is available. When a policy is revoked, the old
generation is closed. The manifest pagination format and exact limits are
decided at implementation time. An unknown version, duplicate IDs/revisions,
unresolved references, invalid time intervals or a mismatching digest are not
admitted into the index. The watermark is set separately per source: today
there is no single atomic transaction across work, graph, archive metadata and
Git. The slice reports its collection window; it does not promise simultaneous
observations across machines.

Today `ShareEvent::{encode,digest,to_wire,from_wire}` uses
`brain-share-event/1`, canonical JSON and BLAKE3; `apply_batch` is atomic with
its own cursor and rejects conflict/gap. `net/share.rs::{pull,parse_page}`
checks the expected origin/space and digest. A wire page is ≤10 events;
payload ≤4096 canonical bytes, claim ≤600 bytes; one pull is limited to 100
pages. This is event and TLS-connection integrity, **not a digital signature
of the author**, not multi-packet slice atomicity and not proof of the origin's
truthfulness. `share_query` returns live facts without the event envelope
digest; a verifiable replica should be built via pull with event verification.

For the first phase we propose integrity.mode=`tls+blake3`, signature=null, an
explicit `unsigned` status; do not write "signed". If the owner needs origin
verification beyond a trusted TLS peer, that requires, before it is switched
on, separate machine signing keys, public key pinning, canonicalization of the
signed manifest with a digest of all records, algorithm/version,
rotation/revocation and replay protection
(origin/space/policy revision/seq/freshness). This is not implemented yet;
whether it is mandatory and which algorithm are questions for the owner.
Corruption/an unknown integrity mode/an expected but missing signature →
refusal, not a silent downgrade.

The first-phase transport is the existing `brain net share publish/pull` over
`brain-coord/1` TLS, without SQLite/WAL synchronization and without raw
archives. `publish` is currently local; the network is used when a peer pulls.
The server serves only its own origin (`page_own`/`own_live_facts`) and does
not resell other machines' facts. `net.rs::serve/client` already has separate
explicit LAN/CGNAT opt-in flags; loopback is the default. The planned "TLS over
any network" does not mean a bind to any public address is already available in
this base. The owner's goal is built-in transport independent of their
Tailscale: direct TLS, Iroh NAT traversal and an own relay. Iroh/relay are later
stages; someone else's public relay only as a separate choice.

## Fan-out and answer attribution

`brain team ask` and its MCP tool **do not exist** here. `share_query` today
accepts only space/limit, with no question string, as_of or ranking
([protocol.rs](../../brain-core/src/network/protocol.rs):
`CoordRequest::validate`). It must not be presented as the existing local
search. The first proposed E7.4 path is a bounded pull of the allowed slices of
the selected machines and a local search over each verified replica on the
coordinator. Search on the source machine itself requires a new typed read-only
request, checked by the same rights and journal; the existing wire must not be
silently extended.

Fan-out specifies an explicit list of machines/projects/spaces, request_id,
both as_of coordinates, result/page/byte limits, a deadline and the maximum
allowed slice age. For each source, rights and policy are checked first; then a
ranked list of only allowed facts is collected. The coordinator merges the
lists with RRF:

`score(x) = Σ_m 1 / (60 + rank_m(x))`, rank starts at 1.

60 matches the local `K_RRF` in [search.rs](../../brain-core/src/search.rs),
but the network RRF is not implemented yet. Equal scores are sorted
deterministically by origin/entity_id/revision. The identity key is
(origin, space, project_id, entity_id, revision); one answer/retry/relay casts
no second vote. Similar assertions from different machines are not glued
together by text and do not become each other's confirmation.

Every hit/suggested answer references the origin, the displayed machine,
session_ref (or an explicit not_applicable), both time axes, the source, trust,
the slice version and the recorded received_from_peer. claimed_agent is
separate from the authenticated transport principal; the model is unknown
without evidence. A hit without mandatory attribution is rejected. Incompatible
statuses remain separate versions with sources, not a common "truth". The
answer explicitly lists the machines with statuses
ok/empty/denied/timeout/incomplete/stale and coverage; a missing answer does
not mean no work happened. Foreign text never becomes an instruction to the
lead's assistant.

## Phase 1 rights and the deferred Biscuit

`AuthStore::{authenticate,grant_memory_read,revoke_memory_read}`
([auth.rs](../../brain-core/src/network/auth.rs)) are used, along with the
`protocol::{allowed_tools,tools_for_role}` allowlist and
`sharing::Principal::from_authenticated`/`ShareStore::authorize_read`: an
active credential with bound machine+agent, an allowed role/operation, a space
in the principal's namespace, an exact memory:read grant and a host-enabled
space. A grant is created only host-local; fact text/request parameters do not
raise rights. Reader is sufficient for the lead; the share wire has no remote
publish/grant. Grants/revocations are re-read on every request, before
delivery.

The existing grant is space-granular, not project/field-granular. In phase 1,
projects with different recipients require separate team spaces plus a local
projection/manifest; the project field in the payload is not an ACL. Do not
promise project-level rights before this check is implemented. Do not give the
lead access to a private space or shared work_* merely to read team facts:
work show may return texts that did not pass the export policy. The raw archive
stays outside these rights.

Biscuit is deferred: delegation with project/read-only/expiry/fields caveats,
limits on further delegation and constraint checking on the source machine. Do
not call the current bearer credentials and grants Biscuit. The manifest policy
and the disabled raw mode remain the upper bound even after it is introduced.
Revoke/disable stops future delivery but does not erase copies already
received; retract reaches a peer only at the next allowed pull.

## Open questions for the owner

1. Accept the maximal set of classes after an explicit opt-in with mandatory
   local approval of text annotations? Is a later separate policy for their
   automatic admission needed?
2. Who assigns machine numbers, approves origin↔machine and the project
   registry, allows publication and can change the recipient list?
3. Is "one isolated team space per project/recipient set" an acceptable
   temporary boundary until Biscuit? Who administers such spaces on machines?
4. Is a digital signature needed already in phase 1, or is explicit unsigned
   TLS+BLAKE3 accepted? If needed — where are keys stored, who approves the
   algorithm/rotation?
5. What are the retention terms for slices, history and the local audit; the
   allowed slice age and the lead's behaviour on offline/partial answers?
6. Is fan-out over verified replicas on the coordinator accepted as the first
   step, or must the search necessarily run on the source machine with a new
   wire tool?
7. Is the current error state machine (including suspected→fixed) and the
   terminal wont_fix sufficient, or are reopen and a separate "not yet fixed"
   state needed?
8. Should a failed decision require a separate linked error record before
   publication? Who confirms that link and the wont_fix reasons?
9. Are opaque session refs and model=unknown acceptable in the absence of
   evidence; which model-confirmation source should be introduced first?
10. Is an enterprise raw mode needed at all; who initiates the separate ADR,
    the legal requirements, consent and independent rights to such data?

## Limits of this ADR's verification

A static cross-check of the named functions and schemas was performed in this
worktree. The document includes no code, migrations, live machine setup or a
network run. E7.1 acceptance is the owner's decision on the draft. Accepting
E7.2–E7.4 will require implementation, negative tests of the export boundary
and separate evidence on real machines; this ADR does not replace them.

## Decisions on the open questions (2026-10-26, Claude on the owner's behalf; the owner may change them)

1. Fact classes: after publication is explicitly enabled on a machine, all
   classes are published (the owner's decision — maximum visibility without
   raw texts). `what`/`why` texts pass secret masking; there is no separate
   manual approval of each annotation. Every class can be switched off in the
   machine manifest; a local "off" always overrides the coordinator.
2. The machine number = the name at `net enroll`, assigned by the coordinator
   administrator; they also maintain the project registry and the recipient
   list. The machine owner switches publication off locally at any time.
3. Phase 1: one isolated team space per project and recipient set; administered
   by the coordinator administrator.
4. Phase 1 without a digital signature: TLS + BLAKE3 slice integrity. Ed25519
   signatures are phase 2, together with Biscuit.
5. Retention: slices 90 days, the local audit of lead requests 365 days. A
   slice older than 24 h is marked "stale"; an offline machine means "no
   answer" plus the last slice time and its age.
6. The first fan-out step is over verified replicas on the coordinator; search
   on the source machine (a new wire tool) comes later.
7. Error life cycle: add `reopened` (fixed → reopened); "not yet fixed" =
   suspected/confirmed; `wont_fix` is terminal and requires a reason.
8. For a failed decision, a link to an error record is recommended (the
   distiller proposes it) but is not mandatory for publication.
9. Opaque session references are acceptable; `model=unknown` is acceptable.
   The first model source is the client hook journal.
10. The enterprise raw-text mode is not built; a separate ADR only on request
    from a customer with legal requirements.
