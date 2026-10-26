# Journal export to OpenTelemetry GenAI (`brain archive export --format otlp-json`)

[Русская версия](otel-export.md)

Format: OTLP/JSON (`resourceSpans` structure), printed to stdout. The network is
not used. A session is a trace; every journal row is a span in that trace.
Time comes from `said_at` (readable UTC form of `timestamp`), nanoseconds.

## Mapping of archive roles → OTel GenAI

| Archive role | Span name | `gen_ai.operation.name` | Attributes/events | Note |
|---|---|---|---|---|
| `user` | `chat <provider>` | `chat` | `gen_ai.input.messages` (JSON array, part `type=text`, `content` = row text), `brain.archive.role` | the content is already secret-masked text |
| `assistant` | `chat <provider>` | `chat` | `gen_ai.output.messages` (role `assistant`) | |
| `tool_call` | `execute_tool brain.archive.tool_call` | `execute_tool` | `brain.archive.role`, `brain.archive.content` | the tool name from the standard is unavailable — the journal does not store the name separately, so `gen_ai.tool.name = brain.archive.<role>` |
| `tool_result` | `execute_tool brain.archive.tool_result` | `execute_tool` | `brain.archive.role`, `brain.archive.content` | |
| `tool_error` | `execute_tool brain.archive.tool_error` | `execute_tool` | **`error.type = tool_error` (Stable attribute)**, `status.code = 2` (`STATUS_CODE_ERROR`) | |
| `reasoning` | `chat <provider>` | `chat` | `gen_ai.output.messages` (role `assistant`) | there is no direct standard for a "visible reasoning summary" — it is exported as an assistant message |

Message attributes sit on the span of the corresponding row, as does
`gen_ai.request.model` (label `<provider> conversation`: the actual model is not
recorded in the archive). `kind = 1` (`INTERNAL`), `status.code = 0` (`UNSET`)
or `2` (`ERROR`) — numeric enums.

Common attributes: `gen_ai.provider.name` = client (claude/codex/hermes),
`gen_ai.conversation.id` = session id (trace id = first 32 hex characters of the
session SHA-256; span id = first 16 hex of the SHA-256 of `session:entry_id` —
deterministic, a repeated export yields the same ids), `service.name = brain-ed`.

## Statuses per the standard

Current source: the `open-telemetry/semantic-conventions-genai` repository (the
opentelemetry.io/docs/specs/semconv/gen-ai page now redirects there).

- `error.type` — **Stable**;
- the `chat` operation — **Development**; `execute_tool` — **Development**;
- `gen_ai.provider.name`, `gen_ai.operation.name`, `gen_ai.conversation.id`,
  `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.call.id`,
  `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` — **Development**
  (an early version of the standard used the `gen_ai.system.message` /
  `gen_ai.user.message` / `gen_ai.assistant.message` / `gen_ai.tool.message`
  events — they migrated into the input/output message attributes);
- provider values (`anthropic`, `openai`, …) — Development; the Brain ED client
  (claude/codex/hermes) is not in that list and is emitted as a custom value —
  the standard allows this (custom value MAY be used).

## Filters and boundaries

- `--session ID` — only this session; `--since`/`--until` — window over `said_at`
  (ISO-8601 or Unix seconds; until is an exclusive bound);
- `--no-content` — the `content` column is not read at all; the attribute
  `brain.archive.content_omitted = true` is added on every span; there is no
  text in the document;
- the archive is opened through the common protected path of `archive read/search`:
  the file type, owner, permissions, sidecar files and supported schema version
  are checked;
- quarantined rows are not exported (as in search/read);
- nothing is sent over the network: OTLP/JSON is printed to stdout; shipping to
  a collector is a separate future decision.
