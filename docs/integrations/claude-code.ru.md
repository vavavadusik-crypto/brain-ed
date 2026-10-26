[EN version](claude-code.en.md)

# Подключение Claude Code

Что делает `brain init --agents` для Claude Code, как проверить подключение и как откатить. Все пути — в домашнем каталоге пользователя; сама команда `brain` — стабильная копия, которую установщик кладёт в `~/.brain/bin`.

## Что пишется

- `~/.claude.json` — ключ "mcpServers"."brain" — команда `<brain> serve` (user-scoped MCP)
- `~/.claude/settings.json` — ключ "hooks" — семь быстрых хуков: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PreCompact, Stop, SessionEnd (все вызывают `brain hook --event … --client claude`)
- `~/.claude/CLAUDE.md` — инструкции памяти между маркерами <!-- BRAIN_FOR_AI_BEGIN/END -->

## Хуки

Хуки: да, семь.

## Как проверить

```console
$ brain doctor
{
  "settings_agents": [ { "id": "claude", "connected": true }, … ],
  "clients": [ { "label": "Claude Code", "mcp_config_entry_present": true, … } ]
}
```

`brain doctor` работает только на чтение. Строка `settings_agents` показывает
`connected: true`, если в манифесте установки есть записи для файлов этого
клиента; `mcp_config_entry_present` — что запись Brain найдена в конфиге клиента.
Альтернатива в окне настроек: `brain ui` → раздел «Агенты» — там же кнопки
«План изменений» (дифф как `--dry-run`) и подключение с подтверждением.

## Как отключить или откатить

```console
$ brain uninstall --dry-run      # показать план восстановления
$ brain uninstall --yes          # восстановить исходные файлы из резервных копий
Uninstalled Brain integration from 12 file(s).
```

`brain uninstall` восстанавливает содержимое файлов на момент установки
(контрольные суммы в манифесте), созданные Brain файлы удаляет. Отключить
одного клиента можно из окна настроек (`brain ui` → «Агенты» → «Отключить») —
тот же код удаления, но только для выбранного агента; записи остальных
остаются в манифесте.

## Частые проблемы

- **Каталог клиента перенесён (например, `CLAUDE_HOME` указывает в другое место)** — Такой клиент пропускается планировщиком с пометкой relocation — ничего не пишется; верните переменную на место или согласуйте расположение.
- **`an installation manifest already exists and files drifted`** — После установки файлы менялись вручную. Посмотрите `--dry-run`, верните изменения или выполните `brain uninstall`, затем подключите заново.
- **Хуки не срабатывают** — Перезапустите клиент: конфиги читаются при старте. Проверьте `brain doctor` (hook_config_marker_present) и журнал `brain journal …`.
- **`file changed after installation; leaving it untouched` при uninstall** — Файл правился после установки; удаление пропускает его намеренно. Верните файл или правьте вручную.
- **Клиент не виден в `brain init --agents`** — Детект — по каталогу клиента или исполняемому файлу в PATH; создайте каталог (`{dir}`) или установите CLI клиента.

Полная интеграция: MCP, хуки и инструкции памяти.

Общий порядок: `brain init --agents --dry-run` → просмотреть дифф → `brain init --agents --yes` (или подтвердить в окне настроек).
