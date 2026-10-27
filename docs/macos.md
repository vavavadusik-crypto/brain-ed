# Brain ED on macOS / Brain ED на macOS

Status / Статус: prepared for first native CI, **not yet verified on a Mac**.
Linux remains the verified platform until both macOS jobs pass. See the
[portability map](portability.md) and [release build guide](build-release.md).

## Русский

Данные остаются в `~/.brain`; переноса в `~/Library` нет. Установщик размещает
только бинарный файл в `~/.local/bin/brain`, без sudo, моделей и служб.
Используйте установщик из проверенного checkout и явно выберите версию,
для которой уже опубликованы Darwin-ассеты. Наличие workflow ещё не означает,
что такие ассеты есть у существующего релиза.

```bash
bash install.sh --version 0.2.0 --dry-run
# Только после публикации и проверки нужного релиза:
bash install.sh --version 0.2.0
export PATH="$HOME/.local/bin:$PATH"
brain --version
brain --offline init
```

Для Apple Silicon нужен `brain-<v>-aarch64-apple-darwin`, для Intel —
`brain-<v>-x86_64-apple-darwin`. Сначала проверяется SHA-256 из `SHA256SUMS`,
затем бинарник атомарно устанавливается; установщик его не запускает.
`--dry-run` не обращается к сети и не создаёт файлов. Удаление
`bash install.sh --uninstall` сохраняет все данные `~/.brain`.
Штатного Bash 3.2 достаточно.

Бинарники **без Developer ID подписи и нотариализации**. Для ARM64 возможна
техническая ad-hoc подпись линкера; это не доверенная подпись издателя.
Gatekeeper может запретить запуск загруженного файла. После самостоятельной
проверки источника и SHA-256 можно снять quarantine **только с этого файла**:

```bash
xattr -d com.apple.quarantine "$HOME/.local/bin/brain"
```

Не отключайте Gatekeeper целиком. Если атрибута нет, команда `xattr` сообщит
об этом; обычная установка через curl не обязана добавлять quarantine.

Сборка из исходников требует уже установленного Rust из
`rust-toolchain.toml` и Apple Command Line Tools. Конвейер использует native
Apple clang, без Zig/musl и без universal2. Отдельный релизный сборщик:

```bash
cargo build --locked --workspace --all-features
cargo test --locked --workspace --all-features
bash packaging/build-macos.sh
```

Планировщики archive, backup, distill и team publish используют приватные
plist `~/Library/LaunchAgents/org.brain-ed.*.plist`. Команды `archive timer`,
`backup timer`, `distill schedule` и `team schedule` сохраняют прежний CLI.
Загрузка: `launchctl bootstrap gui/<uid> <plist>`, выгрузка:
`launchctl bootout gui/<uid>/<label>`. Нужна пользовательская GUI-сессия;
SSH-only/headless запуск может оставить plist и вернуть
`files_written_launchctl_failed`. После смены пути бинарника обновите таймер.
`active` означает «загружен», не «прямо сейчас работает».

Ограничения первой версии порта:

- Восстановление каталогов использует `RENAME_SWAP`; на неподдерживаемой ФС
  операция завершается ошибкой без неатомарного fallback. `flock` — advisory,
  он **не запрещает SQLite-клиенту открыть файл**. Перед `backup restore --yes`
  остановите все Brain/SQLite процессы; предупреждение есть и в stderr, и в JSON.
  Старое дерево и исходный snapshot сохраняются.
- Hook stdin: обычный файл и stream socket поддержаны. Blocking pipe/TTY
  пропускаются: запись события/brief может отсутствовать при обычном pipe-вызове
  клиента. Это честная деградация до реализации независимого deadline-reader.
- TLS/credential fd на Darwin дублируется только для обычного файла:
  чтение сдвигает общий offset с передавшим его процессом, флаги не меняются.
- Приватность файлов: POSIX owner/mode и no-follow проверки сохранены.
  Group-writable родительский каталог не считается приватным по `/etc/group`:
  на macOS источником групп является Directory Services.
- Self-copy и фоновые процессы используют путь current_exe. В отличие от Linux,
  поколение заменённого бинарника не закрепляется через `/proc/self/exe`.
  Перед переустановкой интеграций перезапустите Brain.
- Автоматическая остановка старого UI-окна недоступна; закройте его вручную.
  Проверка поколения coordinator-процесса возвращает unknown.
- Launchd StartInterval не равен systemd OnUnitInactiveSec: интервалы считаются
  иначе и могут объединяться после сна; backup запускается с периодом 24 часа,
  без обещания systemd Persistent/catch-up. Nice=10 сохранён, IOSchedulingClass
  и systemd TimeoutStartSec не воспроизводятся. Environment shell не наследуется:
  если провайдеру нужен API key env, настройте его отдельно в launchd-сессии;
  ключи не записываются в plist.
- Архивному collector нужен уже установленный Python 3; на Apple Silicon
  проверяется `/opt/homebrew/bin/python3`, затем `/usr/local/bin/python3` и
  `/usr/bin/python3`. Установщик Brain Python не скачивает.
- `.app`/Finder launchers, подпись и нотариализация не входят в эту работу.
  Linux XDG desktop installer не является macOS установщиком. Пути конфигов
  сторонних клиентов ещё не проверены на реальных macOS-установках.

CI запускает Rust, общие Python/JS-регрессии и macOS-тесты на ARM64 и Intel.
Linux-only Python-тесты явно skipped с причиной. macOS-13 снят с обслуживания,
поэтому матрица использует macos-15 и macos-15-intel; детали в
[карте переносимости](portability.md#m1-implementation-status).

## English

The macOS port is ready for its **first CI run**, with no claim of native test
success yet. Data stays in `~/.brain`; the download installer writes only
`~/.local/bin/brain`, preserving all data on uninstall. Use a reviewed checkout,
select a version whose Darwin assets have actually been published, and inspect
`bash install.sh --version 0.2.0 --dry-run` before installation. A workflow does
not imply existing releases already contain macOS binaries.

Assets are `brain-<v>-aarch64-apple-darwin` (Apple Silicon) and
`brain-<v>-x86_64-apple-darwin` (Intel). The installer verifies SHA256SUMS before
atomic placement and never executes the payload. Stock Bash 3.2 is sufficient.
Source builds need the pinned Rust toolchain and Apple Command Line Tools
already installed. `packaging/build-macos.sh` builds a native per-architecture
Mach-O binary with Apple clang; it does not create a universal2 binary.

Releases have **no Developer ID signature or notarization** (ARM64 may carry a
linker-generated ad-hoc signature). Gatekeeper may block downloaded files.
After independently verifying the source and checksum, remove quarantine from
only that executable with `xattr -d com.apple.quarantine "$HOME/.local/bin/brain"`.
Do not disable Gatekeeper globally. The attribute may not exist for curl downloads.

All four opt-in schedulers use private `org.brain-ed.*.plist` LaunchAgents in
`~/Library/LaunchAgents`, with literal ProgramArguments, HOME/BRAIN_HOME pinned,
`launchctl bootstrap gui/<uid> <plist>` and `bootout gui/<uid>/<label>`. A logged-in
GUI session is required. A failed bootstrap is reported as
`files_written_launchctl_failed`, not successful activation. Status “active”
means loaded, not necessarily running. Shell environment variables (including
provider API-key variables) are not inherited; configure the login launchd
environment separately. Keys are never copied into plists.

Known limits:

- Restore uses atomic `renamex_np(RENAME_SWAP)` and advisory flock. **Stop every
  Brain/SQLite client first**: flock cannot exclude ordinary opens or idle WAL
  holders as Linux file leases do. Unsupported filesystems fail without a
  non-atomic fallback. The source snapshot and displaced home are retained.
- Hook stdin supports regular files and stream sockets; blocking pipes/TTY are
  skipped so parent descriptor flags stay untouched and the hook does not hang.
  Normal client pipe invocations may therefore omit journaling/project briefing.
- Inherited TLS/credential descriptors must be regular files. Darwin duplicates
  share the caller's file offset; reads advance it, while flags remain unchanged.
- POSIX ownership, private modes and symlink checks remain enforced. Darwin does
  not trust group-writable parents using incomplete `/etc/group` membership data.
- Executable copy/reexec uses current_exe paths and cannot pin a replaced Linux
  image generation. Restart before updating integrations. Close stale UI windows
  manually; coordinator process-generation diagnostics report unknown.
- StartInterval differs from systemd timing and may coalesce ticks after sleep.
  Backup uses a 24-hour interval, without Persistent/catch-up parity. Nice=10 is
  retained; IOSchedulingClass and systemd job timeouts have no parity here.
- The archive collector requires an existing Python 3 in `/opt/homebrew/bin`,
  `/usr/local/bin` or `/usr/bin`; the installer does not download Python.
- No `.app`/Finder launcher, Developer ID signing or notarization. Linux XDG
  launchers remain Linux-specific. Actual third-party client config paths still
  need validation on real Macs.

Native CI covers ARM64 and Intel using macos-15 and macos-15-intel. The requested
macos-13 label is retired; macos-14 is also approaching retirement. Linux-only
fault-injection, ELF-bundle and systemd tests carry explicit skip reasons;
Darwin tests exercise swaps, advisory locking, plutil validation, mock launchctl,
secure opens, inherited TLS descriptors and hook behavior. ABI/linking,
sqlite-vec/Apple clang, APFS behavior and Gatekeeper remain native-run risks.
