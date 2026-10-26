# Экспорт журнала в OpenTelemetry GenAI (`brain archive export --format otlp-json`)

[English version](otel-export.en.md)

Формат: OTLP/JSON (структура `resourceSpans`), печать в stdout. Сеть не
используется. Сессия = trace, каждая запись журнала = span в этом trace.
Время — из `said_at` (читаемая UTC-форма `timestamp`), наносекунды.

## Соответствие ролей архива → OTel GenAI

| Роль архива | Span name | `gen_ai.operation.name` | Атрибуты/события | Примечание |
|---|---|---|---|---|
| `user` | `chat <provider>` | `chat` | `gen_ai.input.messages` (JSON-массив, part `type=text`, `content` = текст записи), `brain.archive.role` | содержимое — уже замаскированный текст |
| `assistant` | `chat <provider>` | `chat` | `gen_ai.output.messages` (role `assistant`) | |
| `tool_call` | `execute_tool brain.archive.tool_call` | `execute_tool` | `brain.archive.role`, `brain.archive.content` | имя инструмента из стандарта недоступно — journal не хранит имя отдельно, поэтому `gen_ai.tool.name = brain.archive.<роль>` |
| `tool_result` | `execute_tool brain.archive.tool_result` | `execute_tool` | `brain.archive.role`, `brain.archive.content` | |
| `tool_error` | `execute_tool brain.archive.tool_error` | `execute_tool` | **`error.type = tool_error` (атрибут Stable)**, `status.code = 2` (`STATUS_CODE_ERROR`) | |
| `reasoning` | `chat <provider>` | `chat` | `gen_ai.output.messages` (role `assistant`) | прямого стандарта для «видимого резюме рассуждений» нет — отображается как assistant-сообщение |

Атрибуты сообщений находятся на span соответствующей записи, как и
`gen_ai.request.model` (метка `<provider> conversation`: настоящая модель в архиве не записана).
`kind = 1` (`INTERNAL`), `status.code = 0` (`UNSET`) либо `2` (`ERROR`) — числовые enum.

Общие атрибуты: `gen_ai.provider.name` = клиент (claude/codex/hermes),
`gen_ai.conversation.id` = session id (trace id = первые 32 hex символа
SHA-256 сессии; span id = первые 16 hex от SHA-256 `session:entry_id` —
детерминированно, повторный экспорт даёт те же id), `service.name = brain-ed`.

## Статусы по стандарту

Актуальный источник: репозиторий
`open-telemetry/semantic-conventions-genai` (страница
opentelemetry.io/docs/specs/semconv/gen-ai теперь перенаправляет туда).

- `error.type` — **Stable**;
- операция `chat` — **Development**; `execute_tool` — **Development**;
- `gen_ai.provider.name`, `gen_ai.operation.name`, `gen_ai.conversation.id`,
  `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.call.id`,
  `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` — **Development**
  (ранняя версия стандарта использовала события `gen_ai.system.message` /
  `gen_ai.user.message` / `gen_ai.assistant.message` / `gen_ai.tool.message` —
  они мигрировали в атрибуты input/output messages);
- значения провайдеров (`anthropic`, `openai`, …) — Development; клиент
  Brain ED (claude/codex/hermes) в этот список не входит и отдаётся как
  собственное значение — стандарт это допускает (custom value MAY be used).

## Фильтры и границы

- `--session ID` — только эта сессия; `--since`/`--until` — окно по `said_at`
  (ISO-8601 или Unix секунды; until — исключающая граница);
- `--no-content` — колонка `content` не читается вовсе, добавляется атрибут
  `brain.archive.content_omitted = true` на каждом span; текста в
  документе нет;
- архив открывается общим защищённым путём `archive read/search`: проверяются
  тип файла, владелец, права, sidecar-файлы и поддерживаемая версия схемы;
- карантинные записи не экспортируются (как в search/read);
- ничего не отправляется по сети: OTLP/JSON печатается в stdout, отправка в
  коллектор — будущее отдельное решение.
