# Brain ED

![Brain-shaped project map on synthetic example data](docs/images/brain-map-synthetic.png)

The image uses twelve synthetic projects. It illustrates the project map; it
does not show a deployed fleet or complete conversation-history graph.
[Русская версия](README.ru.md) · [Documentation](docs/README.md)

**Brain ED is a local-first memory and task coordinator for AI coding agents:
your agents remember what happened, share a task board, and answer from your
own history — everything stays on your machine.** Written in Rust on SQLite;
the command and package are named `brain`.

- **Durable memory with provenance.** Attributed, bi-temporal records, a code
  graph, full-text and optional vector search — no more "what did we decide
  last week".
- **Team mode over end-to-end TLS.** Several machines publish typed facts
  (errors, decisions, plans) to a coordinator and query each other; enroll →
  join → publish is four commands, revoked with one.
- **Honest boundaries.** Opt-in capture, masked secrets, explicit network
  opt-ins, and limits stated in the documentation instead of hidden.

**0.2.0 — early preview. Linux x86_64 and aarch64 musl release targets**
(including Android/Termux aarch64). See [verification limits](docs/VALIDATION.md).
Binaries and `SHA256SUMS` are attached to each [release](https://github.com/vavavadusik-crypto/brain-ed/releases).
macOS and Windows are planned, not supported yet — see the
[portability map](docs/portability.md).

## Quick start in 5 minutes

Needs Bash, curl and `sha256sum` (or `shasum`), Linux x86_64/aarch64.
On Android, use Termux (`pkg install bash curl coreutils`); install into Termux's
private HOME, not shared Android storage. This path needs no Rust or sudo.
Five minutes is the target for the basic setup, excluding optional model downloads;
it is not a measured promise for every network or device.

Download `install.sh` from the same reviewed source tag or checkout as the release.
Inspect the script before running it. From a checkout:

<!-- quickstart:begin -->
```sh
git clone https://github.com/vavavadusik-crypto/brain-ed.git
cd brain-ed
bash ./install.sh --version 0.1.4 --dry-run
bash ./install.sh --version 0.1.4
export PATH="$HOME/.local/bin:$PATH"
brain init --agents --dry-run       # inspect agent config changes first
brain init --agents                # review, then confirm interactively
# For explicit non-interactive approval after review: brain init --agents --yes
brain remember "Build uses cargo --locked" --kind decision --tag demo
brain --offline recall "cargo locked"  # find the memory just written
brain ui
```
<!-- quickstart:end -->

The installer downloads the matching static ELF and `SHA256SUMS` over HTTPS,
checks the digest, then installs only `~/.local/bin/brain`. It never runs the
downloaded binary. Checksums detect corrupted downloads; the manifest from the
same release is not an independent signature. Installation does not configure
agents, collect archives, start services or download models. `brain init --agents`
is a separate confirmed step; client conflicts and supported integrations are
listed by the preview before anything is written.

## What it does

- **Memory and search.** A single-user SQLite store (`~/.brain/brain.sqlite`)
  with attributed, bi-temporal records (`brain remember`, `brain recall`),
  BM25 + exact-entity search and optional vector search (`brain ask`), a code
  graph from `brain index`, and per-source counts and freshness (`brain stats`).
- **Agent session journal.** `brain archive …` keeps the visible conversation
  text of Claude Code, Codex and Hermes (every Hermes profile) plus bounded,
  secret-masked journal rows — tool calls, results, errors, visible reasoning.
  Collection is opt-in and role-filtered; see [docs/journal.md](docs/journal.md)
  and [docs/session-archive.md](docs/session-archive.md).
- **Distillation.** `brain distill …` turns selected archived sessions into
  attributed, versioned facts through a configured LLM (local llama.cpp, an
  HTTPS gateway, or Claude Code/Codex subscription adapters — the latter have
  no tools and a read-only sandbox respectively). Bounded batches, persisted
  cursors, provider fallback, an optional timer. Verified walkthrough:
  [docs/distill-guide.md](docs/distill-guide.md); data flow and limits:
  [docs/distiller.md](docs/distiller.md).
- **Task board.** `brain work …` and the `work_*` MCP tools give cooperating
  agents tasks, claims, fences and leases in
  `~/.brain/coordination/coord.sqlite`. It never intercepts edits: an agent
  that never asks is not stopped. See [docs/coordination.md](docs/coordination.md).
- **Team mode and demo.** `brain net init` → `net serve` → `net enroll` →
  `net join` connects machines over TLS (LAN, VPN or a company relay) with `0600` credentials;
  `brain team …` publishes opt-in typed facts per space and answers queries
  across machines. Ten-minute walkthrough with verified output:
  [docs/team-mode.md](docs/team-mode.md); one-command synthetic demo:
  `tools/team-demo.sh` (see [docs/team-mode-facts.md](docs/team-mode-facts.md)).
- **Settings window.** `brain ui` serves a loopback-only settings page (search
  profile, memory knobs, agent connect/disconnect with a diff and explicit
  confirmation, archive privacy, coordinator state) plus the memory graph
  (`--page graph`) and the 3D viewer with energy-saving modes.
  See [docs/graph-explorer.md](docs/graph-explorer.md).
- **Backup.** `brain backup …` creates consistent snapshots (`0600`), lists,
  prunes and atomically restores them; a daily timer is opt-in.
  See [docs/backup.md](docs/backup.md).
- **OpenTelemetry export.** `brain archive export` renders the journal as
  OpenTelemetry GenAI JSON (sessions as traces, entries as spans, no network).
  See [docs/otel-export.md](docs/otel-export.md) ([in English](docs/otel-export.en.md)).
- **MCP server.** `brain serve` runs a stdio JSON-RPC MCP server; the tool
  surface is the same one the agents use. See [docs/mcp-and-hooks.md](docs/mcp-and-hooks.md).

## Connecting agents

`brain init --agents --dry-run` previews every change; nothing is written
without review and confirmation. Step-by-step guides per client (setup, checking, rollback):
[Claude Code](docs/integrations/claude-code.en.md) · [Codex](docs/integrations/codex.en.md) ·
[Cursor](docs/integrations/cursor.en.md) · [OpenCode](docs/integrations/opencode.en.md) ·
[OpenClaw](docs/integrations/openclaw.en.md) · [Kimi Code](docs/integrations/kimi-code.en.md). Supported integrations:

- **Claude Code** — user-scoped MCP registration, seven fast-path hooks, memory
  instructions in `~/.claude/CLAUDE.md`;
- **Codex** — MCP server and hooks in `~/.codex/config.toml` plus a hooks file
  and memory instructions;
- **Cursor** — MCP registration and memory rules (Cursor has no supported
  global hook config);
- **OpenCode** — MCP registration and memory instructions (plugins only);
- **OpenClaw** — MCP registration;
- **Kimi Code** — MCP registration (its hooks cannot add context).

`brain clients register`/`configure` record a client home explicitly; a
registered Codex home can opt into MCP. Registration alone does not connect or
capture. See [docs/client-profiles.md](docs/client-profiles.md),
[docs/registered-codex-mcp.md](docs/registered-codex-mcp.md) and
[docs/mcp-and-hooks.md](docs/mcp-and-hooks.md).
`brain doctor` reports the whole picture read-only (integrations, timers,
providers, disk space).

## Security and privacy, briefly

- **Local first.** Everything lives under `~/.brain`; no telemetry, no model
  weights, no credentials ship with the source.
- **Secrets are masked on write** into the archive journal — pattern-based,
  with a shared Python/Rust test corpus; masking **can miss** secrets, so treat
  the archive as sensitive anyway.
- **The network is always an explicit opt-in**: the coordinator serves loopback
  TLS with enrolled credentials, LAN listening is experimental and separate,
  fact sharing is default-deny per space, and distillation sends selected text
  only to the provider you configured.
- **Capability limits are stated**, not hidden: see the list below and
  [docs/SECURITY.md](docs/SECURITY.md).

### Capability limits (read before relying on it)

- **Tested locally, not a fleet.** Coordination and sharing are tested with
  local processes and on a three-machine stand (Linux laptop, ARM phone in
  Termux, cloud VM) over a private overlay network. That is not evidence of a large
  fleet, LAN deployment or enterprise readiness. LAN listening exists only as
  an experimental, TLS-only, explicit opt-in for one private IP; see
  [docs/network-transport.md](docs/network-transport.md).
- **Activity is agent-declared.** The activity view and "active work" show
  what agents reported through hooks and claims. Brain ED does not watch the
  screen, keystrokes or file system.
- **Archive capture is partial and masked, not safe.** The collector stores the
  visible conversation text plus bounded (2 KB) journal rows, masked at write
  time. System prompts, injected instructions, sidechains, hidden or encrypted
  reasoning are never stored. A complete history is not guaranteed.
- **Archive is personal and global.** It belongs to the account owner, is not
  scoped by project and has no project ACL. Search results are marked untrusted.
- **Security is engineered, not proven.** There are tests for the boundaries;
  there is no formal verification or external audit. See
  [SECURITY.md](SECURITY.md).

## Install from source (Linux)

Use pinned Rust from `rust-toolchain.toml`, C/C++ compilers and Cargo.lock:

```sh
cargo build --locked --release -p brain-cli
mkdir -p "$HOME/.local/bin"
cp "${CARGO_TARGET_DIR:-target}/release/brain" "$HOME/.local/bin/brain"
```

The previous source/desktop installer is retained as
`packaging/install-source.sh --dry-run`, then `packaging/install-source.sh`.
It installs the desktop copy in `~/.brain/bin`; see [source installation](docs/INSTALL.md).
Old dynamic glibc bundles have a separate [bundle contract](docs/linux-download.md).
Maintainers: [static release build instructions](docs/build-release.md).

## Platforms

- **Linux x86_64 and aarch64** — supported release targets (static musl).
- **Android/Termux aarch64** — supported; install into Termux's private HOME.
- **macOS and Windows** — planned, not supported. The full portability survey
  (every Linux-specific place, the replacement APIs, effort and risk) is in
  [docs/portability.md](docs/portability.md).

## Quick commands

All examples use synthetic data. `brain --help` and `brain <command> --help`
are authoritative; the generated reference is in
[docs/cli-reference.md](docs/cli-reference.md).

```console
brain init                                   # create/migrate ~/.brain/brain.sqlite
brain remember "Build uses cargo --locked" --kind decision --tag build
brain recall "cargo locked"                  # search records written with remember
brain index ./example-project                # code graph
brain ask "where is the config parser"       # code + memory + archive search
brain stats                                  # per-source counts and freshness
brain doctor                                 # integrations, timers, providers, disk
brain ui                                     # settings page on 127.0.0.1
brain work board --ready                     # tasks ready to claim
brain archive status                         # what the collector saw and stored
brain archive search "example error" --limit 5 --role tool_error
brain backup create                          # consistent snapshot
brain distill run --dry-run                  # preview fact extraction
brain init --agents --dry-run                # preview agent-config changes
```

## Documentation

- [docs/VALIDATION.md](docs/VALIDATION.md) — release checks and verification limits
- [docs/INSTALL.md](docs/INSTALL.md) — install, launchers, uninstall
- [docs/SECURITY.md](docs/SECURITY.md) — threat model and data boundaries
- [docs/cli-reference.md](docs/cli-reference.md) — every command and option, generated from `--help`
- [docs/coordination.md](docs/coordination.md), [docs/network-transport.md](docs/network-transport.md),
  [docs/team-mode.md](docs/team-mode.md), [docs/team-mode-facts.md](docs/team-mode-facts.md),
  [docs/shared-memory.md](docs/shared-memory.md) — coordination, TLS, team mode, sharing
- [docs/session-archive.md](docs/session-archive.md), [docs/journal.md](docs/journal.md),
  [docs/otel-export.md](docs/otel-export.md) — archive, journal, OTel export
- [docs/distill-guide.md](docs/distill-guide.md), [docs/distiller.md](docs/distiller.md) — distillation
- [docs/backup.md](docs/backup.md) — backup and restore
- [docs/portability.md](docs/portability.md) — macOS/Windows portability map
- [docs/memory-map.md](docs/memory-map.md), [docs/graph-explorer.md](docs/graph-explorer.md) — work map and graph pages
- [CHANGELOG.md](CHANGELOG.md), [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)

## Licence and contributing

Apache-2.0 (see [LICENSE](LICENSE)). Third-party components keep their own
licences; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Contributions
are welcome: read [CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/VALIDATION.md](docs/VALIDATION.md) first — every user-visible change is
expected to come with tests that fail without it.

[Memory brain explorer: controls, regions and limits](docs/graph-explorer.md) · [Experimental remote MCP reads](docs/remote-mcp.md)

[Development roadmap](ROADMAP.md)
