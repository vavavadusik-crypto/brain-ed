> For the checksum-verified download installer (`./install.sh`) and Linux musl /
> Termux setup, see the [five-minute quick start](../README.md#quick-start-in-5-minutes).
> This page documents the retained source/desktop installer.

# Installing Brain ED 0.2.0 (early preview source)

Supported: **Linux (x86_64 glibc)** from source. macOS and Windows are not
supported in this preview: the source uses Unix-only APIs and `/proc/self`
ownership checks, and CI runs only on Linux. Use a matching prebuilt bundle
only when that release lists one; otherwise build from source. A bundle
installs the version it was built from. See
[Prebuilt bundle](#prebuilt-bundle-only-when-a-release-lists-one).


## Prerequisites

| Need | Why |
|---|---|
| Rust via rustup | `rust-toolchain.toml` pins the exact toolchain; rustup installs it on first `cargo` call. Workspace edition is 2024. |
| C and C++ compilers (`cc`, `c++`) | Bundled SQLite and sqlite-vec use C; esaxx-rs uses C++. |
| Network access to crates.io | `cargo build --locked` fetches exactly the versions in `Cargo.lock`. |
| `python3` (optional) | Only for `tools/session_archive.py` / `tools/agent_bridge.py`. The built-in `brain archive` collector does not need it. |
| `gio` (optional) | GNOME: lets the binary mark desktop launchers as trusted. |

No root access is needed or used.

## Installer

```console
./packaging/install-source.sh --help
./packaging/install-source.sh --dry-run     # prints the commands; no build, download or file write
./packaging/install-source.sh               # build + launchers
./packaging/install-source.sh --uninstall   # launchers out; ~/.brain kept
```

`--uninstall --dry-run` lists what would be removed and changes nothing.

`./packaging/install-source.sh` does exactly:

1. Refuses to run as root, on non-Linux, or without an absolute `$HOME`.
2. From the checkout root (so `rust-toolchain.toml` applies even when the
   script is started elsewhere): `cargo build --locked --release -p brain-cli
   --target-dir <checkout>/target`. The explicit target directory overrides an
   inherited `CARGO_TARGET_DIR`. Default features: the Model2Vec runtime is
   compiled in, **no weights are downloaded**.
3. `target/release/brain ui --install-shortcut`. The binary itself:
   - copies itself to `~/.brain/bin/brain` (the stable copy launchers and agent
     hooks point at, so `cargo clean` does not break them) and probes it;
   - writes two entries to `$XDG_DATA_HOME/applications` (default
     `~/.local/share/applications`): `brain-for-ai-settings.desktop` and
     `brain-for-ai-graph.desktop`, opening `brain ui --port 8787` and
     `brain ui --port 8787 --page graph`;
   - writes two SVG icons under the hicolor theme;
   - if an XDG desktop directory exists, also writes both entries there, marks
     them executable and tries `gio set … metadata::trusted true`.

Any failing step stops the script with a non-zero exit code; the binary writes
no launcher if copying or probing the stable copy fails. Paths are quoted, so
checkouts and home directories containing spaces work.

### Desktop behaviour

- **No desktop directory.** If `XDG_DESKTOP_DIR` (from
  `$XDG_CONFIG_HOME/user-dirs.dirs`) is missing or equals `$HOME`, the XDG rule
  is "no desktop": entries go to the application menu only, and the JSON report
  says `desktop_entry_written: false`.
- **"Untrusted launcher".** If `gio` is absent or the desktop ignores the
  attribute, the report says `marked_trusted: false`. Right-click the icon and
  choose **Allow Launching** (GNOME) or the equivalent.
- The launchers use port 8787 on 127.0.0.1; a second click reuses the server.

### Manual install (same result)

```console
cargo build --locked --release -p brain-cli
target/release/brain ui --install-shortcut --dry-run   # paths only
target/release/brain ui --install-shortcut
```

Without launchers, run `~/.brain/bin/brain` or `target/release/brain` directly
or put either on `PATH`.

## Prebuilt bundle (only when a release lists one)

Use a Linux x86_64 bundle only if the GitHub release page lists
`brain-ed-<version>-linux-x86_64.tar.gz`; otherwise install from source as
above. A bundle installs the version it was built from; at this stage the
release page lists the available versions. After unpacking, run
`./install-linux.sh --dry-run`, then
`./install-linux.sh`. The installer checks the unpacked files against their
checksums before it runs the bundled binary, refuses root, and then runs
the same `brain ui --install-shortcut` step, producing the same two
launchers. `./install-linux.sh --uninstall` removes the launchers and the desktop binary
copy when no managed agent installation owns it; databases and settings are kept.
The binary is a dynamic glibc build; distributions other than the one it
was built on are untested. The checksums detect damage only; they are not
provenance. Format, checks and limits:
[docs/linux-download.md](linux-download.md).

## Optional components

- **Embeddings:** `brain model install` (asks first; `--yes` approves
  non-interactively). `--offline` / `BRAIN_OFFLINE=1` forbids even that.
  Model licences are shown on the model page and are yours to accept.
- **Agent integration:** separate, explicit step; see
  [Connecting agents](#connecting-agents-claude-code-codex-cursor) below.
- **Archive collector:** off until `brain archive enable`; `brain archive timer
  install` adds a systemd user timer. Both are separate explicit steps.

## Connecting agents (Claude Code, Codex, Cursor)

Check the exact flags of your build first: `brain init --help`,
`brain uninstall --help`, `brain serve --help`.

1. **Preview.** `brain init --agents --dry-run` detects the user configs and
   prints every proposed diff with its file path. It writes nothing.
2. **Review** the diffs and the per-client paths. Save open work in the
   agents you are about to reconnect.
3. **Apply** only after review: `brain init --agents --yes` applies the
   non-conflicting changes without prompts. The integration report is text:
   read the diffs, file paths and `CONFLICT` diagnostics to see which files
   were written or refused and why. Database initialization emits a separate
   JSON record; it is not a per-client integration report.
4. **Reconnect.** Restart or reconnect the MCP clients (e.g. reopen the
   session) so they load Brain's tools. Do not kill running servers blindly.

If `CLAUDE_CONFIG_DIR` or `CODEX_HOME` points somewhere other than the default
`~/.claude` or `~/.codex`, `brain init --agents` leaves that client alone. It
does not read or write that client's default files, and it prints one
`CONFLICT` line naming the variable and its state, never its value. Other
clients are set up as usual. Relocated homes are not supported: Brain
neither installs into them nor reads them. Hermes has no MCP or hook
integration. Details: [mcp-and-hooks.md](mcp-and-hooks.md).

### What the integrator refuses instead of guessing

It edits only what it can identify exactly, and leaves the rest to you:

- **Codex (TOML).** An existing `[mcp_servers.brain]` table that Brain did not
  write, or the same key spelled differently (dotted, literal `'…'`, basic
  `"…"` or Unicode-escaped), is **refused** with a diagnostic asking for a
  manual edit; it is never duplicated or silently adopted. A file containing
  triple-quoted multi-line values (`"""` / `'''`) is also refused; a comment
  that merely mentions triple quotes does not change that. This is a
  conservative check, not a full TOML parser.
- **Claude Code and Cursor (JSON).** Only the exact root `mcpServers` and
  `hooks` entries are touched; unrelated fields are preserved. Valid JSONC
  comments are accepted. Trailing commas are refused rather than rewritten.
- **Any client.** Duplicate keys in the target object and ambiguous marker
  text are refused. Unreadable or non-UTF-8 files are left unchanged.

### Not all-or-nothing

`brain init --agents` skips conflicting files and may still integrate other
files, including other files for the same client. The selected writes use
backups and rollback on a write failure; a detected conflict does not cancel
those selections. Before retrying, run the dry run again, read the text
report and check each file. Fix the conflict by hand; never chmod
the originals and never delete personal configs or `~/.brain` data to "clear"
a refusal.

### Manual integration

When a file is refused, add the entry yourself, pointing at the stable copy
(not `target/`), with your real home directory instead of `/path/to/user-dir`.
TOML does not expand `~`:

```toml
# Codex
[mcp_servers.brain]
command = "/path/to/user-dir/.brain/bin/brain"
args = ["serve"]
```

```json
{ "mcpServers": { "brain": { "command": "/path/to/user-dir/.brain/bin/brain", "args": ["serve"] } } }
```

Merge the JSON fragment into the existing root object (Claude Code, Cursor);
do not replace the file. Then reconnect the client. Manually added entries
are not in Brain's integration manifest, so `brain uninstall` will not remove
them; remove them by hand.

### По-русски: конфликты и переподключение

1. Сначала `brain init --agents --dry-run`: показывает все изменения и пути
   файлов, ничего не пишет. Проверьте флаги через `brain init --help`.
2. Только после проверки: `brain init --agents --yes`. Прочитайте текстовый отчёт, пути и сообщения `CONFLICT`
   по каждому файлу: что записано, что отказано и почему.
3. Отказ — это не ошибка, а защита: чужая или иначе записанная таблица
   `[mcp_servers.brain]`, многострочные значения в тройных кавычках, висячие
   запятые в JSON, повторяющиеся ключи, неоднозначные маркеры, нечитаемые или
   не-UTF-8 файлы — такие файлы не меняются. Внесите запись вручную (пример
   выше, путь `~/.brain/bin/brain serve` полностью).
4. Конфликтные файлы пропускаются: остальные файлы, включая другие файлы
   того же клиента, могут быть изменены. Для выбранных записей есть резервная
   копия и откат при ошибке записи. Перед повтором снова сделайте `--dry-run` и проверьте
   файлы каждого клиента.
5. Сохраните работу и переподключите или перезапустите MCP-клиенты, чтобы они
   увидели инструменты Brain. Не убивайте серверы вслепую, не меняйте права
   на исходные файлы и не удаляйте личные конфиги и данные `~/.brain`.

## Uninstall

```console
./packaging/install-source.sh --uninstall          # or: brain ui --remove-shortcut [--dry-run]
brain uninstall --dry-run         # agent integrations, if you added them
```

Launcher removal deletes the menu/desktop entries, icons and the shortcut's
ownership marker. It deletes `~/.brain/bin/brain` only when no agent
integration manifest still needs it. Databases, settings, archives and models
under `~/.brain` are never removed; delete that directory yourself if you want
the data gone.

`brain uninstall` skips the entries of a client whose home variable points
elsewhere and keeps the manifest, backups and binary. Running it again with
the variable unset completes the uninstall. Files already restored are
recognised and left as they are, and files you edited stay conflicts.
Entries are matched against paths under the current `HOME`.

## Data locations

| Path | Contents |
|---|---|
| `~/.brain/brain.sqlite` | memory and code graph |
| `~/.brain/coordination/coord.sqlite` | tasks, claims, leases |
| `~/.brain/map/map.sqlite` | work-map ledger |
| `~/.brain/models/` | embedding models, only after `brain model install` |
| `~/.brain/archive/` | private conversation archive (personal, not project-scoped) |
| `~/.brain/bin/brain` | stable binary copy |

`brain archive status` reports the exact archive paths in use.
