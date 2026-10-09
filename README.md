# Brain ED


![Brain-shaped project map on synthetic example data](docs/images/brain-map-synthetic.png)

The image uses twelve synthetic projects. It illustrates the project map; it
does not show a deployed fleet or complete conversation-history graph.
[Русская версия](README.ru.md)

**0.1.1 — early preview. Linux only.**

Brain ED is a local-first personal memory and task-coordination engine for AI
coding agents, written in Rust on SQLite. The command and package are named
`brain`. No model weights, provider credentials or personal archives ship with
the source.

## What it is

- **Personal owner memory.** A single-user SQLite store (`~/.brain/brain.sqlite`)
  with attributed, bi-temporal records, BM25 + exact-entity search, optional
  vector search, and a code graph built from `brain index`.
- **Memory graph and explorer.** `brain ui --page graph` opens a loopback-only
  page over what is stored.
- **Work map (`/map`).** A 2D/3D project map with an eight-region brain
  overview. Only *Projects*, *Active work*, *Plans* and *Coordination* show
  counts, and only for the page of projects actually loaded; the other regions
  say "not here" and link to the graph or the archive. The regions are a way to
  sort memory, not anatomy.
- **Settings UI.** `brain ui` (default page) shows configuration, status and
  agent activity.
- **Tasks, claims and leases.** `brain work …` and the `work_*` MCP tools give
  cooperating agents tasks, claims, fences and leases in
  `~/.brain/coordination/coord.sqlite`. It does not intercept edits: an agent
  that never asks is not stopped.
- **Coordinator over TLS.** `brain net …` serves `brain-coord/1` over loopback
  TLS with enrolled `0600` credentials; opt-in fact sharing (`brain net share`)
  is default-deny per space.
- **Conversation archive.** `brain archive …` keeps visible user/assistant text
  from Claude Code, Codex and Hermes, with opt-in collection.
- **MCP server.** `brain serve` runs a stdio JSON-RPC MCP server.

## Capability limits (read before relying on it)

- **Tested locally, not a fleet.** Coordination and sharing are tested with
  local processes and loopback TLS. That is not evidence of a real distributed
  fleet, LAN deployment or enterprise readiness. LAN listening exists only as
  an experimental, TLS-only, explicit opt-in for one private IP
  (`brain net serve --listen <IP> --allow-lan`, IP SAN = listen IP = client
  `--server-name`, operator firewall required); see
  `docs/network-transport.md`. It is not production-proven.
- **Activity is agent-declared.** The activity view and "active work" show
  what agents reported through hooks and claims. Brain ED does not watch the
  screen, keystrokes or file system.
- **Archive capture is partial.** For all three clients the collector stores
  only visible, allowed message text after you enable it. Thinking/reasoning,
  tool calls and tool output are dropped. It cannot guarantee a complete
  history (missed rotations, deleted or out-of-window files, format changes).
- **Archive is personal and global.** The archive belongs to the owner of the
  account. It is **not** scoped by project and has **no** project ACL. Search
  and read results are marked untrusted; retrieval treats archived text as data,
  never as instructions.
- **Map counts are partial.** Counts cover loaded projects and tasks only;
  memory-record counts per region are not implemented.
- **Security is engineered, not proven.** There are tests for the boundaries
  below; there is no formal verification or external audit. See
  [SECURITY.md](SECURITY.md) and [docs/SECURITY.md](docs/SECURITY.md).

## Install from source (Linux)

Prerequisites: a Rust toolchain via rustup (`rust-toolchain.toml` pins the
exact version; the workspace uses edition 2024), a C compiler for the bundled
SQLite, and `python3` only if you want the optional archive tools.

```console
git clone https://github.com/vavavadusik-crypto/brain-ed.git && cd brain-ed
./install.sh --dry-run     # shows the plan; builds and writes nothing
./install.sh               # cargo build --locked --release -p brain-cli + launchers
./install.sh --uninstall   # removes launchers; keeps ~/.brain
```

Details, desktop-launcher behaviour and manual steps: [docs/INSTALL.md](docs/INSTALL.md).

After installation, add the stable copy to this terminal session:

```sh
export PATH="$HOME/.brain/bin:$PATH"
```

## Quick commands

All examples use synthetic data. `brain --help` and `brain <command> --help`
are authoritative.

```console
brain init                                   # create/migrate ~/.brain/brain.sqlite
brain remember "Build uses cargo --locked" --kind decision --tag build
brain recall "cargo locked"                  # search records written with remember
brain index ./example-project                # code graph
brain ask "where is the config parser"       # code + memory search
brain status                                 # counts, layers, size, freshness
brain doctor                                 # integration/capture coverage, read-only
brain ui                                     # settings page on 127.0.0.1
brain ui --page graph                        # memory graph
brain work board --ready                     # tasks ready to claim
brain work whoami                            # agent + machine identity
brain archive status                         # what the collector saw and stored
brain archive search "example error" --limit 5
brain init --agents --dry-run                # preview agent-config changes
brain uninstall --dry-run                    # preview removal of agent integrations
brain model install                          # optional: explicit embedding download
```

## Documentation

- [docs/VALIDATION.md](docs/VALIDATION.md) — release checks and verification limits
- [docs/INSTALL.md](docs/INSTALL.md) — install, launchers, uninstall
- [docs/SECURITY.md](docs/SECURITY.md) — threat model and data boundaries
- [docs/coordination.md](docs/coordination.md), [docs/network-transport.md](docs/network-transport.md),
  [docs/shared-memory.md](docs/shared-memory.md) — coordination, TLS, sharing
- [docs/session-archive.md](docs/session-archive.md) — archive
- [docs/memory-map.md](docs/memory-map.md) — work map
- [CHANGELOG.md](CHANGELOG.md), [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)

## Licence

Apache-2.0 (see [LICENSE](LICENSE)). Third-party components keep their own
licences; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

[Memory brain explorer: controls, regions and limits](docs/graph-explorer.md) · [Experimental remote MCP reads](docs/remote-mcp.md)
