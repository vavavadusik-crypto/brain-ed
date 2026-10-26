# Документация Brain ED / Brain ED documentation

Начните с руководства на своём языке. Brain ED — локальная память для агентов;
Linux остаётся единственной поддерживаемой платформой. Командный режим,
дистилляция и расписания включаются явно.

Start with the guide in your language. Brain ED is local memory for agents;
Linux is the only supported platform. Team sharing, distillation and schedules
require explicit opt-in. Where one link is shown, the document has no separate
translation yet.

## Начало работы / Getting started

| Тема / Topic | Русский | English |
|---|---|---|
| Обзор и быстрый старт / Overview and quick start | [README RU](../README.ru.md) | [README EN](../README.md) |
| Установка / Installation | [Инструкция / Guide](INSTALL.md) | [Linux download](linux-download.md) |
| Дистилляция за десять минут / Distillation walkthrough | [Руководство](distill-guide.ru.md) | [Guide](distill-guide.md) |
| Командный режим за десять минут / Team walkthrough | [Руководство](team-mode.ru.md) | [Guide](team-mode.md) |
| Все команды / Command reference | [CLI reference](cli-reference.md) | [CLI reference](cli-reference.md) |

## Память и агенты / Memory and agents

- [MCP tools, hooks and session brief](mcp-and-hooks.md),
  [remote MCP](remote-mcp.md).
- [Профили клиентов / Client profiles](client-profiles.md),
  [registered Codex MCP](registered-codex-mcp.md), [agent bridge](agent-bridge.md).
- [Архив разговоров / Conversation archive](session-archive.md),
  [журнал / Journal](journal.md).
- Дистиллятор: [RU](distiller.md), [EN](distiller.en.md).
- Экспорт OpenTelemetry: [RU](otel-export.md), [EN](otel-export.en.md).
- [Бэкап и восстановление / Backup and restore](backup.md).
- [Graph explorer](graph-explorer.md), [memory map](memory-map.md),
  [schema](schema.md).

## Команда и сеть / Team and networking

- [Факты команды: манифест, срез, поиск, MCP и расписание](team-mode-facts.md).
- [Network transport](network-transport.md), [company relay](relay.md).
- [Task coordination](coordination.md), [shared memory](shared-memory.md).
- [Team facts architecture (EN)](adr/0002-team-facts.en.md),
  [coordination architecture](adr-0001-coordination.md).

## Безопасность и выпуск / Security and releases

- [Security and trust boundaries](SECURITY.md),
  [report a vulnerability](../SECURITY.md).
- [Что изменилось / Changelog](../CHANGELOG.md).
- [Release builds](build-release.md), [public source export](public-export.md),
  [portability limits](portability.md), [validation scope](VALIDATION.md).

Архив и ответы моделей/команды — недоверенные данные. Маскировка распознаёт
известные формы секретов; она не гарантирует удаления всех личных данных.
Наличие настройки не доказывает подключение агента, а локальные тесты не
доказывают работу физического флота.

Archive text and model/team answers are untrusted data. Pattern masking does
not guarantee removal of all personal information. Configured integrations do
not prove client handshakes; local tests do not prove physical fleet readiness.
