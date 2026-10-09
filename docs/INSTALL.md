# Installing Brain ED 0.1.0 (early preview)

Supported: **Linux (x86_64 glibc)** from source. macOS and Windows are not
supported in this preview: the source uses Unix-only APIs and `/proc/self`
ownership checks, and CI runs only on Linux. No binary releases exist yet.

## Prerequisites

| Need | Why |
|---|---|
| Rust via rustup | `rust-toolchain.toml` pins the exact toolchain; rustup installs it on first `cargo` call. Workspace edition is 2024. |
| C compiler (`cc`) | `rusqlite` builds the bundled SQLite; `sqlite-vec` is C. |
| Network access to crates.io | `cargo build --locked` fetches exactly the versions in `Cargo.lock`. |
| `python3` (optional) | Only for `tools/session_archive.py` / `tools/agent_bridge.py`. The built-in `brain archive` collector does not need it. |
| `gio` (optional) | GNOME: lets the binary mark desktop launchers as trusted. |

No root access is needed or used.

## Installer

```console
./install.sh --help
./install.sh --dry-run     # prints the commands; no build, download or file write
./install.sh               # build + launchers
./install.sh --uninstall   # launchers out; ~/.brain kept
```

`--uninstall --dry-run` lists what would be removed and changes nothing.

`./install.sh` does exactly:

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

## Optional components

- **Embeddings:** `brain model install` (asks first; `--yes` approves
  non-interactively). `--offline` / `BRAIN_OFFLINE=1` forbids even that.
  Model licences are shown on the model page and are yours to accept.
- **Agent integration:** `brain init --agents --dry-run` previews config diffs
  for Claude Code, Codex and Cursor; `brain uninstall --dry-run` previews their
  removal.
- **Archive collector:** off until `brain archive enable`; `brain archive timer
  install` adds a systemd user timer. Both are separate explicit steps.

## Uninstall

```console
./install.sh --uninstall          # or: brain ui --remove-shortcut [--dry-run]
brain uninstall --dry-run         # agent integrations, if you added them
```

Launcher removal deletes the menu/desktop entries, icons and the shortcut's
ownership marker. It deletes `~/.brain/bin/brain` only when no agent
integration manifest still needs it. Databases, settings, archives and models
under `~/.brain` are never removed; delete that directory yourself if you want
the data gone.

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
