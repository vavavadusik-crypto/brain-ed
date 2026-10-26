# Brain ED

![Карта проектов в форме мозга на искусственных примерах](docs/images/brain-map-synthetic.png)

На снимке — двенадцать искусственных проектов. Это пример карты проектов,
а не подтверждение работы сети компьютеров или полного архива переписок.
[English version](README.md)

**Brain ED — локальная память и координатор задач для ИИ-агентов: ваши агенты
помнят, что было, делят доску задач и отвечают по вашей собственной истории —
всё остаётся на вашей машине.** Написано на Rust поверх SQLite; команда и пакет
называются `brain`.

- **Прочная память с провенансом.** Записи с авторством и двумя осями времени,
  граф кода, полнотекстовый и необязательный векторный поиск — больше не нужно
  вспоминать, «что мы решили на прошлой неделе».
- **Командный режим по сквозному TLS.** Несколько машин публикуют типизированные
  факты (ошибки, решения, планы) координатору и спрашивают друг друга; enroll →
  join → publish — четыре команды, отзыв — одна.
- **Честные границы.** Сбор по явному включению, маскировка секретов, сетевые
  включения — только вручную, ограничения названы в документации, а не спрятаны.

**0.2.0 — ранний предварительный выпуск. Цели релизной сборки: Linux x86_64 и aarch64 musl**
(включая Android/Termux aarch64). См. [границы проверок](docs/VALIDATION.md).
Бинарники и `SHA256SUMS` приложены к каждому [выпуску](https://github.com/vavavadusik-crypto/brain-ed/releases).
macOS и Windows — в планах, пока не поддерживаются: см.
[карту переносимости](docs/portability.md).

## Что это

- **Память и поиск.** Однопользовательская SQLite-база (`~/.brain/brain.sqlite`):
  записи с авторством и двумя осями времени (`brain remember`, `brain recall`),
  поиск BM25 + точные сущности и необязательный векторный поиск (`brain ask`),
  граф кода (`brain index`), счётчики и свежесть по источникам (`brain stats`).
- **Журнал сессий агентов.** `brain archive …` хранит видимый текст переписки из
  Claude Code, Codex и Hermes (включая каждый профиль Hermes) плюс ограниченные
  по размеру и замаскированные журнальные записи: вызовы инструментов, их
  результаты, ошибки и видимые резюме рассуждений. Сбор включается явно и
  фильтруется по ролям; что именно хранится — [docs/journal.md](docs/journal.md),
  [docs/session-archive.md](docs/session-archive.md).
- **Дистилляция.** `brain distill …` превращает выбранные сессии архива в
  версионированные факты со свидетельствами через настроенную LLM (локальный
  llama.cpp, HTTPS-шлюз или адаптеры подписок Claude Code/Codex — у первого нет
  инструментов, у второго read-only-песочница). Ограниченные пакеты, курсоры
  продолжения, fallback провайдеров, необязательный таймер. Проверенное
  руководство: [docs/distill-guide.ru.md](docs/distill-guide.ru.md); передача
  данных и ограничения: [docs/distiller.md](docs/distiller.md).
- **Доска задач.** `brain work …` и MCP-инструменты `work_*` дают агентам задачи,
  захваты, fence и аренды в `~/.brain/coordination/coord.sqlite`. Правки не
  перехватываются: агент, который не спрашивает, не останавливается. См.
  [docs/coordination.md](docs/coordination.md).
- **Командный режим и демо.** `brain net init` → `net serve` → `net enroll` →
  `net join` соединяет машины по TLS (локальная сеть, VPN или relay компании) с учётными данными `0600`;
  `brain team …` публикует включаемые типизированные факты по пространствам и
  отвечает на запросы между машинами. Десятиминутное руководство с проверенным
  выводом: [docs/team-mode.ru.md](docs/team-mode.ru.md); синтетическое демо
  одной командой: `tools/team-demo.sh`
  (см. [docs/team-mode-facts.md](docs/team-mode-facts.md)).
- **Окно настроек.** `brain ui` — страница только на loopback (профиль поиска,
  переключатели памяти, подключение/отключение агентов с диффом и явным
  подтверждением, приватность архива, состояние сети), а также граф памяти
  (`--page graph`) и 3D-вьюер с энергорежимами. См.
  [docs/graph-explorer.md](docs/graph-explorer.md).
- **Резервные копии.** `brain backup …` создаёт согласованные снимки (`0600`),
  показывает, чистит по возрасту и атомарно восстанавливает; дневной таймер —
  по явному включению. См. [docs/backup.md](docs/backup.md).
- **Экспорт OpenTelemetry.** `brain archive export` превращает журнал в
  OpenTelemetry GenAI JSON (сессии — трейсы, записи — спаны; без сети). См.
  [docs/otel-export.md](docs/otel-export.md).
- **MCP-сервер.** `brain serve` — MCP по stdio JSON-RPC; поверхность
  инструментов та же, которой пользуются агенты. См.
  [docs/mcp-and-hooks.md](docs/mcp-and-hooks.md).

## Подключение агентов

`brain init --agents --dry-run` показывает план изменений; без просмотра и
Пошаговые инструкции для каждого клиента (подключение, проверка, откат): [Claude Code](docs/integrations/claude-code.ru.md) · [Codex](docs/integrations/codex.ru.md) · [Cursor](docs/integrations/cursor.ru.md) · [OpenCode](docs/integrations/opencode.ru.md) · [OpenClaw](docs/integrations/openclaw.ru.md) · [Kimi Code](docs/integrations/kimi-code.ru.md).
подтверждения ничего не пишется. Поддержанные интеграции:

- **Claude Code** — регистрация MCP в пользовательском конфиге, семь
  быстродействующих хуков, инструкции памяти в `~/.claude/CLAUDE.md`;
- **Codex** — MCP-сервер и хуки в `~/.codex/config.toml`, файл хуков и
  инструкции памяти;
- **Cursor** — регистрация MCP и правила памяти (поддерживаемого глобального
  конфига хуков у Cursor нет);
- **OpenCode** — регистрация MCP и инструкции памяти (только плагины);
- **OpenClaw** — регистрация MCP;
- **Kimi Code** — регистрация MCP (её хуки не умеют добавлять контекст).

`brain clients register`/`configure` явно записывают каталог клиента;
зарегистрированный Codex может отдельно включить MCP. Одна регистрация не
подключает клиент и не включает захват. См. [профили клиентов](docs/client-profiles.md)
и [подключение Codex MCP](docs/registered-codex-mcp.md).
`brain doctor` показывает всю картину в режиме чтения (интеграции, таймеры,
провайдеры, место на диске).

## Безопасность и приватность, коротко

- **Локальность.** Всё живёт в `~/.brain`; телеметрии нет, веса моделей, ключи
  провайдеров и личные архивы в исходники не входят.
- **Секреты маскируются при записи** в журнал архива — по образцам, с общим
  Python/Rust корпусом тестов; маскировка **может пропустить** секрет, поэтому
  с архивом обращайтесь как с чувствительными данными.
- **Сеть всегда включается явно**: координатор по умолчанию слушает только loopback по TLS с выданными
  учётными данными, LAN — экспериментальная отдельная настройка, обмен фактами
  по умолчанию запрещён для каждого пространства, дистилляция отправляет
  выбранный текст только настроенному вами провайдеру.
- **Ограничения названы**, а не спрятаны: список ниже и
  [docs/SECURITY.md](docs/SECURITY.md).

### Ограничения (прочтите до использования)

- **Проверено локально, это не парк машин.** Координация и обмен проверены на
  локальных процессах и на стенде из трёх машин (Linux-ноутбук, ARM-телефон в
  Termux, облачная VM) через частную сеть. Это не доказательство работы большого
  парка, LAN или готовности для предприятия. Прослушивание в
  LAN есть только как экспериментальная явная настройка: только TLS, один
  частный IP; см. [docs/network-transport.md](docs/network-transport.md).
- **Активность заявляется агентами.** Экран активности и «Активная работа»
  показывают то, что агенты сообщили через хуки и захваты. Brain ED не следит за
  экраном, клавиатурой или файлами.
- **Архив неполный, маскировка не гарантирована.** Сборщик хранит видимый текст
  переписки плюс журнальные записи до 2 КБ, маскируя секреты при записи.
  Системные подсказки, внедряемые инструкции, сайдчейны, скрытые и
  зашифрованные рассуждения не хранятся никогда. Полная история не
  гарантируется.
- **Архив личный и глобальный.** Он принадлежит владельцу учётной записи, не
  разделён по проектам и не имеет ACL проектов. Результаты поиска помечены
  как недоверенные.
- **Безопасность не доказана формально.** Границы покрыты тестами, но
  формальной верификации и внешнего аудита нет. См. [SECURITY.md](SECURITY.md).

## Быстрый старт за 5 минут

Нужны Bash, curl и `sha256sum` (или `shasum`), Linux x86_64/aarch64.
На Android используйте Termux (`pkg install bash curl coreutils`) и его личный
HOME, а не общее хранилище Android. Rust и sudo не нужны. Пять минут — цель
базовой установки без моделей, а не измеренная гарантия для любой сети и телефона.

Возьмите `install.sh` из проверенного исходного тега или checkout той же версии,
прочитайте его перед запуском. Из checkout:

<!-- quickstart:begin -->
```sh
git clone https://github.com/vavavadusik-crypto/brain-ed.git
cd brain-ed
bash ./install.sh --version 0.2.0 --dry-run
bash ./install.sh --version 0.2.0
export PATH="$HOME/.local/bin:$PATH"
brain init --agents --dry-run       # сначала изучить изменения конфигов агентов
brain init --agents                # проверить план и подтвердить интерактивно
# Явное подтверждение без диалога после просмотра: brain init --agents --yes
brain remember "Сборка идёт через cargo --locked" --kind decision --tag demo
brain --offline recall "cargo locked"  # найти только что записанную память
brain ui
```
<!-- quickstart:end -->

Установщик скачивает статический ELF и `SHA256SUMS` по HTTPS, проверяет сумму
и только затем устанавливает `~/.local/bin/brain`. Скачанный бинарь он не запускает.
Суммы обнаруживают повреждение загрузки; манифест из того же выпуска не является
независимой подписью. Установка не меняет конфиги агентов, не собирает архив,
не запускает службы и не скачивает модели. `brain init --agents` — отдельный шаг
с подтверждением. Команды `remember` и `recall` выше не требуют модели или
ключей провайдера. `brain ui` выводит loopback-адрес: откройте его в браузере,
в том числе в Termux.

**Без моделей работают:** init, remember, лексический BM25/точный поиск
(`brain --offline ask`), координация задач и UI. **Необязательный семантический
поиск:** `brain model install` спрашивает подтверждение и скачивает модель
(около 507 МБ многоязычную, около 30 МБ только английскую); это время не входит
в пять минут. Дистилляция требует отдельно настроенной LLM.

`bash ./install.sh --uninstall` удаляет только CLI (сначала можно `--dry-run`);
данные `~/.brain` сохраняются. Для отдельно включённых интеграций и ярлыков
предусмотрены свои команды удаления.

## Установка из исходников (Linux)

Нужны закреплённый Rust из `rust-toolchain.toml`, компиляторы C/C++ и Cargo.lock:

```sh
cargo build --locked --release -p brain-cli
mkdir -p "$HOME/.local/bin"
cp "${CARGO_TARGET_DIR:-target}/release/brain" "$HOME/.local/bin/brain"
```

Прежний установщик исходников и ярлыков сохранён как
`packaging/install-source.sh --dry-run`, затем `packaging/install-source.sh`.
Он ставит desktop-копию в `~/.brain/bin`; [подробности](docs/INSTALL.md).
У прежних динамических glibc-архивов [отдельные условия](docs/linux-download.md).
Для сопровождающих: [сборка статических релизов](docs/build-release.md).

## Платформы

- **Linux x86_64 и aarch64** — поддерживаемые релизные цели (статический musl).
- **Android/Termux aarch64** — поддерживается; ставьте в личный HOME Termux.
- **macOS и Windows** — в планах, не поддерживаются. Полный обзор переносимости
  (каждое Linux-специфичное место, чем заменять, трудоёмкость и риск) — в
  [docs/portability.md](docs/portability.md).

## Быстрые команды

Примеры на синтетических данных. Точный список — `brain --help` и
`brain <команда> --help`; сгенерированный справочник —
[docs/cli-reference.md](docs/cli-reference.md).

```console
brain init                                   # создать/мигрировать ~/.brain/brain.sqlite
brain remember "Сборка идёт через cargo --locked" --kind decision --tag build
brain recall "cargo locked"
brain index ./example-project                # граф кода
brain ask "где парсер конфига"        # код + память + архив переписок
brain stats                           # источники, счётчики, свежесть
brain doctor                          # интеграции, таймеры, провайдеры, диск
brain ui
brain work board --ready
brain archive status
brain archive search "пример ошибки" --limit 5 --role tool_error
brain backup create                   # согласованный снимок
brain distill run --dry-run           # предпросмотр извлечения фактов
brain init --agents --dry-run
```

## Документация

- [docs/VALIDATION.md](docs/VALIDATION.md) — проверки выпуска и границы проверок
- [docs/INSTALL.md](docs/INSTALL.md) — установка, ярлыки, удаление
- [docs/SECURITY.md](docs/SECURITY.md) — модель угроз и границы данных
- [docs/cli-reference.md](docs/cli-reference.md) — все команды и опции, сгенерировано из `--help`
- [docs/coordination.md](docs/coordination.md), [docs/network-transport.md](docs/network-transport.md),
  [docs/team-mode.ru.md](docs/team-mode.ru.md), [docs/team-mode-facts.md](docs/team-mode-facts.md),
  [docs/shared-memory.md](docs/shared-memory.md) — координация, TLS, командный режим, обмен
- [docs/session-archive.md](docs/session-archive.md), [docs/journal.md](docs/journal.md),
  [docs/otel-export.md](docs/otel-export.md) — архив, журнал, экспорт OTel
- [docs/distill-guide.ru.md](docs/distill-guide.ru.md), [docs/distiller.md](docs/distiller.md) — дистилляция
- [docs/backup.md](docs/backup.md) — резервные копии и восстановление
- [docs/portability.md](docs/portability.md) — карта переносимости на macOS/Windows
- [docs/memory-map.md](docs/memory-map.md), [docs/graph-explorer.md](docs/graph-explorer.md) — карта работы и страницы графа
- [CHANGELOG.md](CHANGELOG.md), [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)

## Лицензия и вклад

Apache-2.0 (см. [LICENSE](LICENSE)). Сторонние компоненты — под своими
лицензиями, см. [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Вклад
приветствуется: сначала прочитайте [CONTRIBUTING.md](CONTRIBUTING.md) и
[docs/VALIDATION.md](docs/VALIDATION.md) — каждое видимое пользователю изменение
ожидается вместе с тестами, которые падают без него.

Проверки и точные ограничения выпуска: [docs/VALIDATION.md](docs/VALIDATION.md).

[3D-граф памяти: управление, области и ограничения](docs/graph-explorer.md) · [Экспериментальное чтение удалённого MCP](docs/remote-mcp.md)

[План развития](ROADMAP.ru.md)
