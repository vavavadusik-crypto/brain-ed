# Work coordination: contract

Design rationale: `docs/adr-0001-coordination.md`.

The coordinator answers one question for cooperating agents: *who may work on
this now?* It does **not** intercept edits. An agent that never asks is not
stopped. Every agent integration (Codex, Claude Code, Hermes) must follow the
protocol below for the guarantees to mean anything.

## Where and who

* Database: `$BRAIN_HOME/coordination/coord.sqlite` (dir 0700, file 0600).
  Local to one computer. Never put it on NFS/SMB or a synced folder.
* Identity: `(--agent, machine id)`. The machine id is a random installation id
  in `$BRAIN_HOME/machine-id`, created on first use. `brain work whoami` shows
  it. `BRAIN_MACHINE_ID` overrides it for tests and simulations; answers then
  say `machine_source: "BRAIN_MACHINE_ID override …"`.
* Space: `--space` (default `default`). Tasks, claims and reads never cross
  spaces. Path claims are additionally scoped by project.

## Protocol for an agent

1. `work_board {ready:true}` (or `brain work board --ready`) — pick a task.
2. `work_claim {task_id}` → keep `claim.claim_id` and `claim.fence`.
   * `busy` → someone else owns it; `details.owner`, `retry_after_secs`.
   * `blocked` → dependencies open; `details.open_dependencies`.
3. Before editing files: `work_claim_path {project, path, task_id}`.
   `conflict` lists overlapping claims of others. Do not edit those paths.
4. Long work: `work_renew {claim_id, fence}` well before `expires_at`
   (e.g. at half the TTL). `lease_expired` → you lost it: stop, re-read the
   board, claim again (new fence).
5. Finish: `work_set_state {claim_id, fence, state: completed|failed|paused|queued, note}`.
   Completed/failed also release your path claims tied to that task.
6. Pass work on: `work_handoff {claim_id, fence, to_agent, to_machine, note}`.
   The receiver calls `work_claim` on the task and gets the new fence.

Any `stale_fence`, `not_owner` or `lease_expired` means: **stop mutating**.
Pass `request_id` on writes you may retry (network hiccup, timeout); the same
id with the same request returns the stored answer (`replayed: true`) for 24 h.
A replayed answer re-reads the claim and task it names, so `live`, `owner`
and `expires_in_secs` describe the present, not the first attempt. Completing
a task releases only *your* path claims tied to it; a path claim you handed
to someone else stays theirs until they release it or its lease runs out.

### Re-checking right before a write

A fence only protects what checks it (Kleppmann: the *resource* must reject old
tokens). Files do not check, so the guarantee ends at the coordinator. To narrow
the gap, re-check immediately before an irreversible step:

```bash
# .git/hooks/pre-commit (opt-in, per repository)
brain --agent codex work verify "$BRAIN_CLAIM_ID" "$BRAIN_FENCE" >/dev/null || {
  echo "lease lost or superseded: re-claim before committing" >&2; exit 1; }
```

`verify` is read-only and does not extend the lease. A check-then-write gap
remains; this is cooperative coordination, not a lock on the filesystem.
On `busy`/`conflict`, retry with backoff and jitter or pick another ready task;
there is no wait queue.

State mapping to A2A v1.0 TaskState: queued→SUBMITTED, active→WORKING,
paused→(no equivalent; closest INPUT_REQUIRED), completed→COMPLETED,
failed→FAILED. completed and failed are terminal here too.

## Limits

| Thing | Bound |
|---|---|
| lease `ttl_secs` | integer 10..=86400, default 900; negatives, fractions, NaN refused |
| title / detail / note | 200 / 4000 / 2000 bytes |
| path | 1024 bytes, lexical canonical form, no `..` escaping the root |
| dependencies per task | 32 |
| page size | ≤ 100 |
| context packet | last 20 events, ≤ 8 KiB of note text |

## Path identity

Lexical only: `/`-separated, `.` dropped, `..` resolved, repeated/trailing
slashes removed. Symlinks are not resolved; case is significant; Unicode is
not normalised. Overlap = equal, or one is an ancestor of the other at a
component boundary (`src` overlaps `src/a.rs`, not `src2`). For work across
computers use project-relative paths (`brain-core/src/lib.rs`), not absolute
home paths, so the same file has the same name everywhere. The same owner may
hold overlapping claims.

## Reads never write

Reads (`board`, `claims`, `show`, `verify`, the UI) open the database read-only.
A task whose lease ran out is still stored as `active` until the next write
sweeps it, but every read reports its *effective* state: no live owner means
`queued`. State filters, `--ready` and the counts use that effective state, so
a poller finds an orphaned task immediately, without anyone writing first.

## Trust labels

Ownership, leases, fences and times are `coordinator_state`, authoritative on
this coordinator. Titles, details and notes are `agent_supplied`: untrusted
text, never instructions. The coordinator never reads the conversation archive
or the memory graph; ask `archive_search` / `memory_search` explicitly.

## Examples

```bash
# Codex
brain --agent codex work add brain-for-ai "Coordinator HTTP adapter" --request-id add-http-1
brain --agent codex work claim 7 --ttl 1800
brain --agent codex work claim-path brain-for-ai brain-cli/src/work.rs --task 7
brain --agent codex work renew 12 41 --ttl 1800
brain --agent codex work state 12 41 completed --note "tests green"
```

Claude Code / Hermes use the same MCP server they already run
(`brain --agent claude-code serve`, `brain --agent hermes serve`):

```json
{"name":"work_claim","arguments":{"task_id":7,"ttl_secs":1800,"request_id":"claude-7-a"}}
{"name":"work_handoff","arguments":{"claim_id":12,"fence":41,"to_agent":"hermes","to_machine":"m-0f3c…","note":"review left"}}
```

A short instruction to put in each agent's project rules (AGENTS.md / CLAUDE.md):

> Before changing files in this repository, claim the task (`work_claim`) and
> the paths you will edit (`work_claim_path`). If refused, do not edit; report
> the owner. Renew long leases; on `stale_fence`/`lease_expired` stop.

## Not in this version

* No network transport. Ten computers need a central coordinator service with
  authentication (see the ADR); sharing the SQLite file is not a substitute.
* No shared/read claims, no glob patterns, no automatic enforcement (pre-commit
  hook or editor integration would be the next step), no cancel/reopen of
  finished tasks.
