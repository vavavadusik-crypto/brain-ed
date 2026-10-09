# Shared memory foundation — 2026-10-07

The user's target is continuity between Codex, Claude Code and Hermes Agent:
original conversations, work, unsuccessful attempts, explicit decisions, project
state and handoffs, with compact evidence-backed retrieval. The current graph
and imported notes are useful, but do not meet that capture contract.

## Decision

Retain Rust + SQLite. Separate original evidence, normalized events, and
rebuildable projections (facts, search, project/task graph). Keep the original
language and technical identifiers; measure bilingual retrieval before choosing
translation. Never turn retrieved conversation text into system instructions.
No mandatory cloud API, LLM extraction or model download in the capture path.

Claude Opus 5.5 independently reviewed this direction on medium effort. Its
architecture recommendations are proposals, not verification of competitor
implementations. Source licenses and runtime costs must be checked separately.

## First implementation slice

1. Fix concurrent JSONL writes and bounded tail reading, expose malformed records
   and clock skew rather than silently misrepresenting liveness.
2. Add a read-only `brain doctor`: metadata capture is distinct from transcript
   preservation; configuration presence is distinct from a runtime handshake.
3. Register Claude user MCP in `.claude.json`; leave hooks in
   `.claude/settings.json`. Preserve installer dry-run and restoration.
4. Correct documentation which still promises automatic SessionStart briefs.

## Following slices

1. Opt-in, independently reversible Claude/Codex/Hermes transcript adapters:
   bounded reads, stable source identity, offsets, rotation handling, SHA-256,
   transactional cursor and archive commit, idempotent replay, private modes.
   Raw local archive is separate from text eligible for cloud retrieval. Report
   gaps, unsupported formats and unavailable history instead of claiming 100%.
   An explicit, manually invoked prototype exists: see `session-archive.md`.
2. Explicit project/task/attempt/decision/handoff events with provenance,
   supersession and failure reasons. Preserve history; never infer a task is
   complete only because the agent stopped or claimed success.
3. Progressive retrieval: small index -> selected records -> original evidence.
   Adapters expose the same protocol through MCP and CLI.
4. Project overview -> task dependencies/timeline -> evidence panels. Graphify
   and the old operational brain remain sources; a million-point graph is not
   the default navigation.

## Acceptance

First slice: multi-process append has no interleaved records; a single event is
visible; truncated/malformed/future events do not look like complete live
coverage; diagnostics neither open the database for writing nor start models or
servers; installer preserves unrelated settings and restores fixtures.

Later slices: restart/rotation/replay without missing or duplicate records;
Claude decision retrieved in Codex and Hermes with the right source; superseded
decision and failed attempt remain discoverable; Russian paraphrases and mixed
code identifiers tested on a held-out query set; report recall@5, provenance
accuracy, p50/p95 latency, cold/warm RSS and context bytes separately. Resource
targets are targets until measured. No quantum simulation is justified by this
workload.
