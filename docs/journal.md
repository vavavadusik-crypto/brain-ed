# The conversation journal: what the archive stores per role

This file describes what the conversation archive keeps beyond the visible
chat, how big each piece may be, how secrets are masked, what is never stored,
and which switches turn things off. The collector itself is
`tools/session_archive.py`; storage lives in `$BRAIN_HOME/archive/sessions.sqlite`.

Every archive row is **untrusted evidence**: search and read results are marked
as such, and retrieval treats archived text as data, never as instructions.

## Roles and what each stores

| Role | Source | Text stored |
|---|---|---|
| `user` | visible user messages (all clients) | the message text as shown |
| `assistant` | visible assistant messages (all clients) | the message text; leading `<think>…</think>` blocks are stripped |
| `tool_call` | Claude `tool_use` blocks, Codex `function_call`/`custom_tool_call`/`local_shell_call`, Hermes `tool_calls` JSON | `"<function name> <arguments>"`, one row per call |
| `tool_result` | Claude `tool_result` blocks, Codex call outputs, Hermes rows with role `tool` | Claude/Codex: the output text; Hermes: `"<tool_name>: <content>"` |
| `tool_error` | Claude `tool_result` blocks marked `is_error` | the error output |
| `reasoning` | Claude `thinking` blocks, Codex `reasoning` **summary** | only the summary the client itself shows |

Hermes sessions from `profiles/<name>/state.db` are stored with session ids
prefixed `profiles/<name>/`, so ids stay unique across profiles; Hermes tool
calls and tool results are really stored today (`_hermes_journal`). Only
Claude contributes `tool_error` (its `tool_result` rows carry `is_error`).
The current archive answer carries the raw client `timestamp` plus a readable
`said_at` (`YYYY-MM-DDTHH:MM:SSZ` UTC, null when the stored time does not
parse) in `archive search`, `archive read` and the neighbour lists.

## Size bounds

- Each journal row keeps at most **2048 characters** (`JOURNAL_MAX`); the rest is
  replaced by a `…[N more characters not archived]` marker.
- One source line / Hermes content value is at most 1 MiB (`MAX_LINE`); a
  single import scans at most 64 MiB (`MAX_SCAN`); Hermes reads at most
  200 000 rows per import (`HERMES_MAX_ROWS`).
- A collector run is bounded by `max_sources_per_run`, `max_bytes_per_run`,
  `max_bytes_per_source`, `max_records_per_run`, `max_seconds` and
  `hermes_rows_per_run` (all configurable in `collector.json`).
- Search excerpts are cut to 300 characters; `archive read` pages text in
  bounded slices.

## Secret masking

Masking is **pattern-based** (`SECRET_PATTERNS`, `MASK_REV = "mask:2"`), applied at
write time to conversation text and every journal row. The shapes below are pinned
exactly by `tests/test_session_journal.py` and the shared vectors
`tests/secret_cases.json`:

- `-----BEGIN … PRIVATE KEY-----` blocks: the whole block becomes `[REDACTED]`;
- bare provider tokens: `sk-`/`rk-`/`pk-` (with optional `ant-`/`proj-`/`live-`/
  `test-` segment, 16+ tail), `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_`
  (20+ tail), `xox[abposr]-` (10+ tail), `AKIA` + 16, `AIza` + 30, JWT `eyJ…` —
  the whole token becomes `[REDACTED]`;
- **assignments to a credential-naming key** — env and JSON forms alike:
  `DB_PASSWORD=…`, `GITHUB_TOKEN: …`, `AWS_SECRET_ACCESS_KEY=…`,
  `"api_key": "…"` — the key stays readable, only the value becomes
  `[REDACTED]` (6+ value characters, quotes excluded, plain numbers kept);
- `Authorization: Basic …` / `Bearer …` and `user:password@` inside a URL —
  the scheme/user/host stay readable, the credential becomes `[REDACTED]`.

Left readable on purpose: `max_tokens=4096`, `tokenize='unicode61 …'`,
`NOPASSWD:`, `credentials: 'include'`, bare numbers, Cyrillic prose, ports in
URLs. Because masking is pattern-based, a credential matching no pattern is
stored as-is — **treat the archive as containing secrets** and keep its `0600`
permissions either way.

Rows stored before the current patterns existed are re-masked in place once
(`mask_rev` in `archive_meta`); no source is re-read. If two stored versions of
one record become identical after masking, the later copy is deleted instead of
kept (the identity is unique).

## The Markdown importer is narrower

The Rust importer behind `brain import --from markdown` redacts only whole
lines containing `sk-`, `ghp_`, `github_pat_`, `AKIA` or a PEM block today
(`brain-cli/src/import.rs`); env-style and JSON assignments are not yet masked
there. Parity with the shapes above is task E1.2.

## What is never stored

- System prompts, developer messages, injected instructions and context blocks
  (Codex `*.instructions` kinds, `<environment_context>` and friends).
- Sidechain conversations the person never saw (Claude `isSidechain`).
- Hidden or encrypted reasoning: Codex `analysis`-type channels, encrypted or
  raw reasoning content (only the visible summary is kept), and the Hermes
  `reasoning` column entirely — Hermes gives no way to tell a visible summary
  from hidden reasoning, so the column is never read (fail closed). An
  unclosed leading `<think>` block hides the whole message.
- Tool payloads and rows the client itself hides (any non-NULL Hermes
  `display_kind`), attachments, and metadata events.
- `state-snapshots` backups of Hermes: they duplicate live history and are
  never scanned (the other clients keep no such directory).

## What the ask path does with the archive

`brain ask` and the MCP `memory_search` search the archive with an OR of the
question's content words, drop entries younger than `archive_min_age_minutes`
(default 120) and drop any excerpt that quotes the search invocation itself
(`brain ask "` or `brain recall "`), counting the removals in
`archive_filtered`. A date phrase in the question (Russian or English) scopes
the answer to that period and is reported as `period` / `period_empty`.

## How to turn things off

- **Archive in answers:** `brain config set include_archive false` (or
  `--no-archive` per call). Fresh-entry hiding: `brain config set
  archive_min_age_minutes N` (0 disables the age filter).
- **Collection entirely:** `brain archive disable` stops the collector; the
  timer unit can be removed with `brain archive timer remove`.
- **Per-client:** the `clients` list in `collector.json` limits which clients
  are read; a client not listed is not even discovered.
- **Older history:** the backfill import only runs while its own config says
  `running` — `brain archive backfill stop` stops admitting new old files.
- There is **no separate switch for journal rows alone**: they are part of
  collection once it is enabled. Dropping them would mean a new capture
  revision; if you need a conversation-only archive, say so in an issue.

Nothing here is a completeness claim: rotations, deleted files, formats the
adapters do not know, and files outside the collection window all leave gaps.
`archive status` reports what was actually seen and stored.
