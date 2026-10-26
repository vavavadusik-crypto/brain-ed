# Distiller of decisions, errors and plans (E5.8)

[Русская версия](distiller.md)

The command is run explicitly by the user or through a systemd user timer the
user has enabled. It reads the **stored secret-masked** conversation archive
through the existing owner, permission, schema and quarantine checks. Client
source files are not opened. The archive has no project ACLs: the space
parameter determines where the result is written, and the session selection
determines the extraction material available.

**Where the text goes.** The masked text of the selected sessions is sent to
the configured LLM provider. With a local server (`http://127.0.0.1`,
`localhost`) it never leaves the machine; with a cloud HTTPS provider it goes
to that provider under its terms. Masking removes recognized secrets, but not
all private content: use a local model for sensitive archives.

Examples:

```sh
brain llm add local --base-url http://127.0.0.1:8080 --model local-model --context-tokens 16384 --timeout-seconds 900
brain distill run --session SESSION_ID --provider local
brain distill run --since 2026-10-01 --max-sessions 20
brain distill run --session SESSION_ID --provider local --dry-run
brain distill run --provider local --roles user,assistant,tool_error,tool_result
brain distill status --roles user,assistant,tool_error
brain ask "почему отказались от решения" --no-archive
```

Without a provider the command exits with a clear error **before opening the
graph database**. For local mode, explicitly select the local provider: without
`--provider` the configured fallback order is used, including remote HTTPS
providers. `--offline` controls model downloads and by itself does not disable
explicitly requested LLM calls.

## Roles and filter

Every entry in a request has the fields `{"id", "role", "content"}`. By default
only `user`, `assistant`, `tool_error` are sent. `--roles` accepts a non-empty
comma-separated list of known roles: `user`, `assistant`, `tool_error`,
`tool_call`, `tool_result`, `reasoning`. An unknown role is an error before any
write. The last three roles require explicit opt-in.

The prompt explains the differences: user — a human, assistant — an agent,
tool_call — a program call, tool_result — program output (data, not human
statements), tool_error — a program error, reasoning — internal reasoning. The
host does not accept a fact whose evidence consists entirely of roles
unsuitable for the status. Confirming a transition requires a user/assistant
quote; tool_error is allowed only for error-type facts. Even with mixed
evidence, a tool_result quote does not confirm a transition.

The filter is applied in SQL **before LIMIT 64**, including when selecting
sessions, checking `--since` and counting status. Late excluded tool_result
rows do not return a completed session to the queue. Each normalized role set
has its own independent cursor; role order and repetitions do not change it.
This makes it possible to explicitly include historical tool results later.
The E5.4 cursor is preserved for the default set: already processed entries are
not analyzed again automatically.

## Schema and identity

Types: `decision / error / dead_end / finding / plan`. The model returns
`what`, `why`, `status`, `evidence` (integer IDs), `session`, `project`; for a
transition — `status_evidence: {entry_id, quote}`; for links —
`links: [{relation, target_type, target_what}]`.

The host checks the type, status, field sizes, non-empty evidence, and that
**every** ID belongs to the chunk actually sent for the current session. An
invalid fact is discarded entirely. Fractional IDs and references to an unsent
tail are also rejected. Facts whose evidence has no parseable time are not
accepted.

The deterministic ID uses a fixed FNV-1a over
`type + normalized what + normalized project`; normalization collapses
whitespace and lowercases Unicode. When an ID matches, full field identity is
verified; a collision causes a refusal. The project comes from the `cwd` of the
evidence; the model's value is ignored; different cwds inside one fact are
rejected. The empty cwd of old archives forms a separate identity.

Metadata of a regular graph node:

- `distilled_fact: true`, `kind` (type), `what`, `why`, `status`, `project`;
- `evidence`: the union of the IDs of all accepted evidence;
- `evidence_by_session`: a mapping of sessions to ID lists;
- `archive_session`: the session of the last accepted assertion;
- `when_min / when_max`: the `said_at` range of all accepted evidence.

Any `when` in the model's response is **ignored**, including numbers and
strings outside the range. Said_at is computed by the common archive parser
from `ts` (ISO-8601 or Unix seconds). The initial `valid_from` is the minimum
of the evidence. On a status change, the `valid_from` of the new version is the
maximum of the new assertion's evidence; an assertion older than the effective
status is rejected. When evidence is added with the same status, the start of
the effective version is preserved.

`brain ask` returns facts as ordinary search results with `kind`, `type` and
`status`. The decision/error types use the corresponding NodeKind; the rest use
Memory.

## Versions and allowed transitions

Changing an existing fact happens only through `replace_node`: the logical ID
is preserved, the old fields and evidence remain in the bi-temporal
`node_versions` history. The new version contains the **union** of the old and
new evidence, attributed to sessions. Repeating already accepted evidence does
not create even a new version. New evidence with the same status extends the
provenance while keeping the original what/why.

| Type | From | Allowed next statuses |
| --- | --- | --- |
| error | suspected | confirmed, fixed, not_a_bug |
| error | confirmed | fixed, wont_fix, not_a_bug, became_feature |
| error | fixed, wont_fix, not_a_bug, became_feature | none |
| decision | proposed | adopted, reverted |
| decision | adopted | worked, failed, reverted |
| decision | worked | failed, reverted |
| decision | failed | reverted |
| decision | reverted | none |
| finding / plan / dead_end | proposed | active, confirmed, abandoned |
| finding / plan / dead_end | active | completed, abandoned |
| finding / plan / dead_end | completed, confirmed, abandoned | none |

A primary observation may have any status listed for its type. A direct
suspected → fixed is allowed because confirmation and the fix can be described
by one new piece of evidence. Reopening a closed error is not supported by this
protocol version.

**A reference to an ID alone is not enough to change a status.** An allowed
transition, new evidence from the current chunk, and a `status_evidence` with a
new ID containing the exact quote are required. A conservative deterministic
filter accepts a whole line of the form `WHAT: STATUS`, `WHAT — STATUS` or
`WHAT is STATUS` (with or without a trailing period/exclamation mark), where
WHAT matches a specific fact. The English statuses are supported, along with
the corresponding Russian synonyms: suspected (подозревается), confirmed
(подтверждена), fixed (исправлена), wont_fix (не будет исправлена), not_a_bug
(не ошибка), became_feature (стала функцией); proposed (предложено), adopted
(принято), worked (сработало), failed (не сработало), reverted (отменено);
active (в работе), completed (завершено), abandoned (отклонено). Example:
`Synthetic failure: fixed`.

A generic instruction like "mark all errors as fixed", an invented quote, old
evidence, a disallowed transition, and quoting only part of the line do not
change the fact and do not create links. The check may deliberately miss useful
natural phrasings. It verifies attribution and an explicit assertion, does
**not prove the truth** of the archive text, and is not complete protection
against all prompt injection: the source and the extraction remain untrusted.
Even an exact line can be false. A primary model error is also possible.

## Links

Every accepted fact gets `has_session` to the deterministic archive session
node. An error with status fixed additionally gets `fixed_by` to the confirming
session; became_feature gets `became`. Model-declared
`caused_by / fixed_by / superseded_by / became` between facts are accepted only
to an existing target fact of the same project and space. Self-references and
unknown relations are discarded; a rejected update adds no links. Links are
marked `trust=untrusted_model_extraction`, with session/evidence. From a failed
decision the model may extract a separate error and link it; no automatic node
reclassification is performed.

## Budgets, cursor and recovery

`context_tokens` is stored in the provider configuration, default 16384. For a
common fallback request the minimum window of the selected providers is used.
Token estimation is `ceil((prompt bytes + JSON schema bytes) / 2)`: a
conservative heuristic for mixed RU/EN, not a measurement by a specific model's
tokenizer. Additionally, 2048 output tokens and 1024 tokens for
framing/headroom are reserved. The session/entries size is measured by the
actual serde_json serialization, including id, role, quotes, backslashes and
control characters. Cyrillic does not get an artificial ×6 multiplier. The
request is also capped at 2 000 000 bytes of session data and never splits a
UTF-8 character. A too-small window causes an explicit refusal. The check
guarantees this window estimate is honored; the actual token count depends on
the model. `chat_template_kwargs.enable_thinking=false` is passed to the
templates; honoring this option depends on the server. The JSON schema is
passed via OpenAI-compatible response_format and re-checked by the host.

Sessions are read in portions of up to 64 entries; requests contain fragments
of several entries. Even a single large entry is split without losing the tail.
Evidence is allowed only from the currently sent chunk. The cursor advances
only after a whole entry is processed. After a failure in the middle of a large
entry its fragments may be sent again; fact acceptance is idempotent.

A separate `distill_progress(space_id, session_id, last_entry_id,
completed_at)` table marks fully processed entries. The distill_runs table from
the WIP is not used. Completed sessions are skipped; new appended entries of
the selected roles return them to the queue. For additional role sets the
`distill_filter_progress` table is used, isolated by
space/session/role_filter. `--max-sessions` limits how many sessions are
actually processed; `--since` selects sessions by visible evidence within the
period, after which the whole selected session is processed. An invalid date is
an error. Archive changes/quarantine release behind the cursor do not trigger
an automatic re-analysis.

Graph writes complete up to the cursor. On a partial error, already accepted
facts are kept and the unfinished chunk is retried without duplicating
nodes/versions. A process-level file lock next to the selected database
serializes distillers; other Brain commands are not blocked by this lock. The
graph and the cursor are not one shared transaction.

`distill status` reads the database without writing and counts
completed/remaining sessions **visible now** relative to the current space,
plus live facts by type/status. A missing archive yields an empty queue.
Command reports never print archive text or model answers. The E5.3
protections (URL parsing, no userinfo, localhost pinning, no redirects, and
safe transport errors) are preserved.


## Timeout and progress

`brain llm add ... --timeout-seconds 900` stores `timeout_seconds` on the
provider. The default is 120 seconds, allowed range 10..=3600; old settings
without the field get 120. Under fallback each provider uses its own timeout.
`brain llm test --timeout-seconds ...` keeps an explicit override for
diagnostics. The timeout covers the request and reading the response; an error
does not print the URL, key, archive text or the model's answer. For a slow CPU
model the example above sets 900 seconds. If even that is not enough, raise the
timeout or lower context_tokens.

For each chunk, stderr receives a `state=started` line and then a
`state=finished` or `state=failed` line: the number of sessions in the request
and the ordinal call number, the bytes of serialized session data, elapsed
seconds and the number of facts written (`accepted`). Session IDs/names,
archive text and model answers never appear in the progress. After an error the
cursor does not skip the failed chunk; previously processed chunks keep their
progress.

`--dry-run` reads the selected stream and the existing cursor, computes the
same chunks and returns `sessions`, `chunks`, `calls`, `bytes`, `roles`,
`facts_written=0`. Here `bytes` is the sum of serialized session data,
excluding the constant system prompt, JSON schema and HTTP framing. The command
sends nothing to the provider and does not create/modify the graph, attribution,
progress tables or the lock file. A provider must still be configured: its
window determines the budget. The session/since/max-sessions limits and the
current cursors apply exactly as in a normal run.

## Synthetic comparison

`tools/distill_synthetic_benchmark.py BEFORE_BINARY AFTER_BINARY` runs both
binaries with separate temporary HOMEs and a loopback HTTP stand without a
model. The stand returns an empty fact list; only counters are printed.
Session: 30 alternating user/assistant entries of 640 UTF-8 bytes each and 100
tool_result entries of 4096 bytes, window 16384. The data does not come from
the user's archive. For temporary-file isolation set TMPDIR inside your own
target dir.

Measured for the original `5080cc3` and E5.5 (`6e35369`) on this stand:

| Counter | Before | After |
| --- | ---: | ---: |
| Provider calls | 230 | 1 |
| Largest session data chunk actually sent, bytes | 1 973 | 20 411 |
| Total session data with JSON framing, bytes | 445 077 | 20 411 |
| Total prompts with the instruction, bytes | 606 077 | 21 475 |

The difference reflects both the exclusion of tool_result and the tighter
budget. This is a measurement of packing/call counts, not an assessment of real
model quality and not a promise of CPU speed. Additional process tests check
evidence/status validity on synthetic answers; the real archive and a real
model are not used for verification.

## Failure diagnostics (E5.6)

The final JSON of `distill run` contains `proposed`, `accepted`, `rejected` —
a `{reason: count}` object with no fact content. `facts_written` remains a
compatible synonym of `accepted`. For every successfully processed chunk and
for the whole run:
`proposed = accepted + sum(rejected.values())`. In stderr the same fields are
added to the progress lines; `state=started` contains zeros. `--dry-run` also
returns zeros. Accepted counts both new facts and accepted provenance/status
updates. Only the first detected reason is counted per fact; storage and
provider errors still fail the command and do not advance the chunk cursor.

| Reason | Meaning |
| --- | --- |
| unknown_type | Unknown type |
| invalid_fields | Missing field or wrong type of a required string field |
| empty_what | Empty what after whitespace collapsing |
| field_too_long | what > 2000 or why > 8000 UTF-8 bytes |
| unknown_status | The status is not allowed for the selected type |
| session_mismatch | The session is not in the sent batch |
| no_evidence | No non-empty evidence array |
| invalid_evidence | An evidence ID is not representable as an i64 integer |
| evidence_outside_chunk | At least one ID was not sent in the current chunk of the specified session |
| tool_result_only | No evidence from user/assistant, or tool_error for an error |
| mixed_projects | The evidence refers to different cwds |
| invalid_timestamp | The time of at least one evidence entry does not parse |
| transition_not_allowed | The transition is not in the transition table |
| transition_unsupported | No new evidence and no exact new quote of an allowed role |
| stale_transition | The new assertion is older than the valid_from of the effective status |
| duplicate | Same status and no new evidence |
| over_max_facts | An item beyond the first 100 in the chunk's response |

Structurally invalid JSON (for example, a missing required field) can be
rejected by the transport validation even before the fact counters; that is a
provider error, not a successful chunk with rejections. The enum is passed to
the provider to constrain the grammar; the host independently verifies
type/status validity.

The prompt and the type/status enums are built from the same tables as the
parsing. The canonical types have Russian synonyms: decision — «решение»,
error — «ошибка», dead_end — «тупик», finding — «находка», plan — «план». The
Russian status equivalents listed above are also accepted in the status field,
normalized to English. For a primary fact, status_evidence is optional; for a
status change it is required. The JSON has the shape `{"facts":[...]}`; the
required fields of each fact: `type`, non-empty `what`, `why`, `status`,
non-empty `evidence`, the exact `session`, `project` (the host replaces the
last one with the value from the cwd).

### Local response storage

`brain distill run --keep-raw /local/private/distill-debug` saves the selected
provider's response text for each chunk into a separate new file.
**The file contains archive-derived material and is for local debugging only.**
Do not add such files to git, reports or logs. Without the flag, responses are
not saved. The message content of the selected provider is saved, without
HTTP headers or keys; fallback attempts rejected by the transport are not
saved.

The parent of the directory must exist. The final directory is created with
0700; an existing one is accepted only when owned by the current process with
0700 permissions. Any symlink in the path and `..` are forbidden. Files are
created via create-new (O_EXCL/O_NOFOLLOW) with 0600 and are never overwritten.
An unusual umask may narrow the permissions further. A saving error stops the
chunk before facts are written and the cursor advances. With `--dry-run` no
directory or files are created.

## Through a subscription

You can use an already authorized Claude Code or Codex CLI without a separate
API key. Brain runs only the built-in `claude` and `codex` adapters, without a
shell or arbitrary commands. Authorization and model availability depend on
your subscription and the client version. Adding the provider by itself does
not launch the CLI.

```bash
brain llm add subscription --command claude --model sonnet --effort medium \
  --context-tokens 128000 --timeout-seconds 900
brain distill run --provider subscription --dry-run --max-sessions 10
brain distill run --provider subscription --max-sessions 10
```

A scheduled run is enabled separately with `brain distill schedule enable`
(see below). Brain does not install a schedule automatically. A repeated run
continues from the saved cursors. On a recognized subscription limit it returns
JSON `status: "provider_limited"` with exit code 0: the cursors of the current
batch are not moved, and previous successful requests keep their progress. A
scheduled run stores this result for `schedule status`, prints no progress to
stderr and does not retry the request in a loop. An unknown client error
remains an error; limit recognition relies on the client's fixed English
categories, not on a guaranteed common protocol across all versions.

**Claude is recommended for an untrusted archive.** It is launched with `-p`,
`--output-format json`, `--json-schema`, `--tools ""`, `--strict-mcp-config`,
an empty `--mcp-config '{"mcpServers":{}}'` and `--no-session-persistence`.
The answer is taken only from the `structured_output` object; the textual
`result` is not accepted as a fact. If the client version does not accept a
protective flag, the run fails; there is no retry with tools enabled. Optional
model and effort are passed as separate arguments. The format and flags were
checked against the
[Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference) and
[headless usage](https://code.claude.com/docs/en/headless); real compatibility
of the installed client requires a separate run by the owner.

An alternative:

```bash
brain llm add subscription-codex --command codex --model YOUR_MODEL --effort low
brain distill run --provider subscription-codex --dry-run --max-sessions 5
brain distill run --provider subscription-codex --max-sessions 5
```

Codex uses `exec --skip-git-repo-check --ephemeral --sandbox read-only`,
`--output-schema`, `-o` and stdin `-`, while effort is passed via
`-c model_reasoning_effort=...`. The model can be omitted to use the client's
settings. Codex has no "no tools" mode: the empty cwd and the read-only sandbox
restrict writes, but do not guarantee isolation from reading the home
directory, user configurations or connected MCP servers. This is a weaker
boundary against prompt injection from the archive. Do not connect privileged
integrations to such a client; use a separate OS profile/account if you need
stronger isolation. See the
[official Codex CLI reference](https://learn.chatgpt.com/docs/cli/reference).

Every invocation gets a new empty cwd with 0700. The Codex schema and result
live in an adjacent private directory, files 0600; after completion the
directory is removed. The prompt is passed only via stdin, with no archive text
in argv. The environment is cleaned to the exact list `PATH`, `HOME`, `LANG`,
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`; API keys, Brain env, the SSH agent and
arbitrary client variables are not inherited. The client keeps access to its
profile via HOME/the configuration directory; cleaning the env does not isolate
files and does not prevent loading user hooks. The cloud provider's privacy and
the subscription spend remain the owner's responsibility.
`--no-session-persistence`/`--ephemeral` do not promise the absence of all
logs or of remote storage at the vendor.

The executable is searched for only in absolute PATH directories. A regular
executable is required, owned by the current user or root, without write
permission for group/others; symlinks are rejected, including npm-shim
symlinks. For such an installation, put a directory with a regular trusted
client executable in PATH. An arbitrary shell string in `--command` is
forbidden. This is a pre-launch file check, not protection against a hostile
same-user process changing PATH/files.

The client's stdout/stderr are not printed: diagnostics contain the category
and exit code. Each stream and the result are capped at 4 MiB; an overflow or
timeout kills the process group. The answer passes the same checks as before:
shape, evidence within the chunk, roles, project, transitions and exact quotes.
Before writing, what/why are additionally processed with `mask_secrets`. This
is heuristic masking of known formats, not proof that no secrets remain.
`--keep-raw` saves the raw result **before** this masking and requires the same
caution as before.

The default window is 128000 tokens for command providers; 16384 for HTTP
providers. `--context-tokens` sets the budget; it does not change the model's
capabilities or its pricing tier. `--dry-run --provider NAME` shows the number
of requests (`calls`, also `chunks`) with batching and the current cursors.
This is the number of planned requests; under fallback one such request may
require several attempts across different providers. The actual subscription
spend depends on the CLI version, model, schema/output, roles, entry sizes and
the answer. The CLI adds its own overhead; the call count is a measurable
indicator but does not give an exact token or cost figure. The real archive and
subscription were not used in the E5.8 verification.

## Session batches (E5.8)

By default the distiller merges neighboring small sessions in the queue while
the actual serialized JSON fits the provider's budget. The maximum is 50
sessions per request; `--batch-sessions N` allows choosing from 1 to 50.
`--batch-sessions 1` restores one session per request, with no merging. A small
session's entries remaining after the cursor are sent whole, even if reading
them spanned several 64-entry pages. A large session that does not fit whole is
processed alone, in chunks; its chunks are never mixed with other sessions.

Request data shape:

```json
{"sessions":[{"session":"session-a","entries":[{"id":1,"role":"assistant","content":"..."}]},{"session":"session-b","entries":[{"id":2,"role":"user","content":"..."}]}]}
```

Every fact must name the exact `session` from the batch. An unknown session
yields `session_mismatch`; evidence of another session (even one present in the
same batch) or an unsent ID yields `evidence_outside_chunk`. Project and time
are computed from that session's evidence. The response format
`{"facts":[...]}` stays the same.

All fact and link writes of a batch must complete successfully before any of
its cursors advances. Then the session cursors are stored in **one SQLite
transaction**. An error in the middle of a write marks no session of the batch
completed; facts already written may remain in the graph and are safely
completed on a retry. There is still no shared transaction between the graph
and the cursors.

By default, sessions with the freshest parseable time of a visible entry of the
selected roles are processed first. ISO-8601 and Unix seconds are compared
after parsing, not as strings; with equal times the session ID decides the
order. Sessions without a parseable time go last. `--oldest-first` reverses
this order (including the position of sessions with unknown time).
`--max-sessions` still limits sessions, not calls.

```sh
brain distill run --provider subscription --max-sessions 100 --dry-run
brain distill run --provider subscription --max-sessions 100 --batch-sessions 10
brain distill run --provider subscription --batch-sessions 1 --oldest-first
```

## Scheduled runs (E5.8)

```sh
brain distill schedule enable --provider subscription --every 1h --max-sessions 20 --quiet-hours 09-23
brain distill schedule status
brain distill schedule disable
```

This is a Linux systemd user timer: `brain-distill.service` and
`brain-distill.timer` in `~/.config/systemd/user`. The same private atomic unit
writing, `ExecStart` argument checks and `systemctl --user` order are used as
for the archive timer: write → daemon-reload → enable --now; when disabling,
first `disable --now`, then unit removal and daemon-reload. Existing data and
cursors are not removed. An systemctl failure returns
`files_written_systemctl_failed` rather than a success message.

A provider is required and must already be configured. The default interval is
`1h`; a positive integer (up to 10000) with a suffix `s`, `m`, `h`, `d` is
accepted. The default session maximum is 20, allowed range 1–1000. The unit
pins the current absolute binary path, database and space; `--offline` is used.
PATH and access to the CLI profile must be available to the user systemd
manager. After moving the binary, enable the schedule again. Running without a
user session requires the owner to configure systemd accordingly; the command
does not change that.

`--quiet-hours 09-23` forbids runs from 09:00 inclusive to 23:00 exclusive
according to the **machine's local clock**. The overnight window `23-09` is
supported; `00-24` forbids runs all day. Without the flag there are no
time-of-day restrictions. The check happens before reading the archive, opening
the graph, the lock and calling the provider. A pass already in progress is not
interrupted when quiet-hours begin.

Each tick makes one bounded pass under the existing lock. The timer waits for
the run to finish, then the `OnUnitInactiveSec` interval; there are no cyclic
retries on a limit. The next tick continues the queue. `schedule status` shows
whether the units exist, `enabled`/`active` from systemctl and the last result
from `~/.brain/distill-schedule-status.json`: `quiet_hours`,
`provider_limited`, `ok` or `failed`. The file is private and holds time and
counters, with no archive text, provider answer or keys. A scheduled run
suppresses progress lines; the result JSON is kept on stdout for the system
journal.

## Call spend: E5.8 synthetic archive

A reproducible run with two built binaries:

```sh
python3 -B tools/distill_synthetic_benchmark.py BEFORE_BINARY AFTER_BINARY --batch-archive
```

The stand creates separate temp HOMEs and SQLite archives: 100 sessions with
one 256-byte ASCII assistant entry each and three sessions with one 60000-byte
entry each (`L`, a quote, a backslash, a newline, repeated 15000 times). The
provider is a loopback HTTP fake with an empty `facts` list, window 16384,
session maximum 1000. No models, real CLIs or user archives are used.

Comparison of the original `05932b2` (contains both Claude-adapter fixes) and
E5.8 `4dcc132` on this stand:

| Metric | Before E5.8 | E5.8 |
| --- | ---: | ---: |
| Calls for 100 small sessions | 100 | 2 |
| Calls for 3 large sessions | 15 | 15 |
| All calls | 115 | 17 |
| Total session data, bytes | 349 922 | 350 275 |
| Total prompts, bytes | 632 132 | 392 656 |
| Largest prompt, bytes | 25 243 | 25 243 |

The call count dropped by 85.2%. The small growth in session data comes from
the new `sessions` envelope; the prompt reduction comes from fewer repetitions
of the system instruction. The prompt-bytes metric excludes the separate JSON
schema, HTTP framing and the subscription client's internal system prompt. The
stand measures batching, not extraction quality or exact subscription
tokens/limits. For a real archive, obtain the call count via `--dry-run` with
the selected provider, role filter, cursors and session limit.
