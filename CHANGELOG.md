# Changelog

All notable changes are listed here. Versions follow semantic versioning;
`0.x` releases may change formats and interfaces.

## [0.1.4] — early preview

### Added
- Explicit registration of Codex, Claude Code and Hermes client homes, with bounded
  owner-only registry state and metadata diagnostics. Registration does not read
  client contents or enable capture.
- Opt-in MCP setup for one registered Codex home (`clients configure` and
  `clients unconfigure`), with dry runs, bounded configuration edits, hash-based
  recovery, drift refusal and a retained integration binary. Claude and Hermes
  registrations do not gain automatic global MCP/hook integration.

### Security and reliability
- Integration commands reject unset, empty, relative, non-UTF-8 or control-bearing
  HOME before writes; `init --agents` checks before database creation. The rule
  covers agent installation/removal, shortcuts and client-profile commands.
- Hook state files reject symlinks, FIFOs, hardlinks and unsafe ownership/modes;
  the handler no longer repairs permissions or truncates a symlink target.
- Hook stdin is bounded by size/time, supports ordinary pipes and stream sockets
  without changing shared descriptor flags, and keeps stdout exactly `{}`.
  Unknown, malformed and oversized input is discarded. The journal remains
  bounded and best-effort, with no complete-capture or real-client callback claim.

### Limits
- Linux x86_64 early preview; no new physical-fleet, enterprise ACL, portability,
  signing/provenance or formal audit claim. Existing integration binaries are not
  silently upgraded or automatically deleted. See the dedicated profile and
  hook documentation for recovery and filesystem timing limits.

## [0.1.3] — 2026-10-10 (early preview)

### Changed
- Relocated agent-client homes are detected and not followed.
  - **Variables:** `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `HERMES_HOME` are
    classified from their raw value in the process environment only. Brain does
    not open, stat or resolve the path, and never prints or logs the value.
  - **States:** `unset`, `empty` and `default_equivalent` keep the old
    behaviour. `unsupported_override` (another absolute path) and
    `unresolved_override` (relative, non-UTF-8 or containing `..`) mark the
    client as relocated.
- `brain init --agents` skips a relocated Claude Code or Codex entirely.
  - None of its default files are read or written, including `~/.claude.json`.
  - It prints one `CONFLICT` line with the client, the variable and the state.
  - Other clients and the stable binary copy are installed as before.
- `brain uninstall` skips the install-manifest entries for a relocated
  client's exact default files before reading anything.
  - Other clients are restored.
  - While anything is skipped, the manifest, backups and stable binary are kept.
- `brain uninstall` can complete after a partial run.
  - A later uninstall counts an entry as already restored when the file holds
    its pre-install bytes again, or, for a file Brain created, is absent.
    Absent means `NotFound` only; an unreadable file is a `CONFLICT`, never
    absence.
  - User edits stay conflicts.
  - Every backup to be used is checked against its SHA-256 before any file is
    changed.
  - Once everything is restored or already restored, the manifest, backups
    and binary are cleaned up. A desktop shortcut that uses the binary still
    keeps it.
  - The dry-run count includes only restores that would happen now.
- `brain uninstall` validates the complete local manifest before acting.
  Only exact managed client paths and bounded, owner-only regular manifest
  and backup files are admitted. Invalid schemas, duplicate paths, unsafe
  backup names and symlinks are refused; client-path symlinks are conflicts.
  These checks restrict accidental or tampered manifests. They do not close
  filesystem races with another process that can modify the same directories.
- `brain doctor` gains three additive fields per client:
  - `config_location` (`{variable, state}`);
  - `mcp_support` and `hook_support`, set to `supported`,
    `relocated_not_followed` or (Hermes) `unsupported`.
  - For a relocated client the existing marker fields are `null`; their type
    is still bool or null.

### Limits
- Matching uses the current spelling of `HOME`. If it changed since install,
  old paths outside the current allowlist refuse the whole uninstall. There is no
  migration, and Brain does not install into, read from or capture from a
  relocated home or a new profile.
- Hermes has no global MCP or hook integration (`unsupported`).
- Configuration markers do not prove a client handshake. Running clients are
  not reconnected. Full conversation capture is not claimed.
- There are no fleet, provenance or signing claims. Linux x86_64 only, with
  dynamic glibc (2.39 on the manual Ubuntu 24.04 workflow). Other distributions
  are untested. Prebuilt availability is listed on the release page.
- Workspace version 0.1.3 (`brain-core`, `brain-cli`). No dependency changes.

## [0.1.2] — early preview (see its GitHub release)

### Added
- Linux x86_64 download bundle module (this entry does not mean a bundle is
  published; use one only if a release lists it):
  `tools/package-linux.py build|verify`, `packaging/install-linux.sh`,
  `docs/linux-download.md` and `tests/test_linux_bundle.py`, plus a manual,
  build-only workflow (`.github/workflows/linux-bundle.yml`). The workflow is
  started by hand, has read-only repository access, uploads a workflow
  artifact and publishes no release.
- The packager bundles an already built `brain` with LICENSE, notices and
  docs read from one exact, clean commit. It refuses, before writing:
  - a dirty or mismatched source;
  - repository configuration that can run programs, worktree configs,
    submodules or extensions, all checked before git runs;
  - an unsafe or existing output;
  - a malformed ELF;
  - any bundled or generated file containing private paths or
    secret-looking tokens.
- `verify` (a packager/CI tool) checks bounded size, canonical unique paths,
  the exact layout and plain file types, a strict `SHA256SUMS`, and a strict
  manifest that agrees with the files and the bundled ELF.
- The bundle installer refuses entries not owned by the user or
  group/world-writable, refuses a `SHA256SUMS` containing NUL bytes or
  changing size, checks every file against a single bounded snapshot of it,
  and runs a private copy of the binary checked against that snapshot. It
  never uses sudo, installs only the two launchers and keeps `~/.brain` data
  on uninstall. It sets up no agents, collector, listener or model download.
  Same-user tampering is not prevented.
- The packager admits only a plain-clone `.git/config` (byte, line and key
  checks) before any git command runs.

### Limits
- Checksums and the manifest detect damage only. The recorded source commit
  is a packaging claim, not build provenance. Nothing is signed.
- Portability is unproven: a dynamic glibc build. The manual workflow builds
  on Ubuntu 24.04 (glibc 2.39) and the manifest records the highest glibc
  symbol version. Other distributions, musl and non-x86_64 are untested or
  unsupported. The workflow's behaviour on GitHub is not verified; as pinned,
  `actions/checkout` can leave `.git/config.worktree`, which the packager
  refuses; it now builds and packages from an ordinary clone of that
  checkout. A real runner check is still needed.
- Workspace version 0.1.2 (`brain-core`, `brain-cli`); no dependency changes.
  Installing from source remains the supported path.

## [0.1.1] — 2026-10-10 (early preview)

### Added and improved
- Opt-in TLS listeners and clients on numeric private LAN addresses. Loopback
  remains the default; LAN needs explicit flags, an IP SAN certificate and
  enrolled credentials. No discovery or firewall changes.
- Five opt-in read-only remote MCP tools for one explicitly configured
  coordinator per call. Private, bounded config; fixed tools; untrusted
  provenance; whole newline-terminated replies capped at 16 KiB. No remote
  writes, local-memory fallback or arbitrary MCP passthrough.
- The memory explorer now places the loaded records in eight brain regions
  by their stored node kind, with an unknown region. Responsive 3D/2D views,
  bounded selection, stable placement, selection details and fit controls.
- Conservative client integration preserves conflicting Codex tables and
  malformed/ambiguous JSON or markers, with manual guidance. Duplicate and
  escaped keys are checked; unsupported triple-quoted TOML values require
  manual integration. Conflicting files are skipped; selected writes keep
  backups and rollback on failure.

### Correctness and limits
- Remote replies distinguish application errors, malformed envelopes and
  transport refusals. Local argument checks reuse the coordinator's project
  rule; duplicate config fields are refused before parsing can overwrite them.
- Remote truncation preserves object keys, counts omissions and bounds the
  trimming work. `truncated: false` does not mean the source list is complete:
  board/claims/facts currently expose only a first page.
- `--offline` still forbids model downloads; it is not a network isolation
  control. Remote reads require the separate `--remote-config` opt-in.
- Linux source preview only. Real TLS and separate synthetic installations
  were tested on one machine; physical LAN/fleet deployment, complete native
  capture and enterprise isolation are not claimed. No binary download yet.

## [0.1.0] — 2026-10-09 (early preview)

First public preview of Brain ED (`brain` CLI). Linux only, source install.

### Included
- Local SQLite memory with attribution, bi-temporal records, BM25/entity
  search, optional Model2Vec embeddings (explicit `brain model install`).
- Code graph indexing (Rust, Python, JavaScript, TypeScript, Go).
- Stdio MCP server, agent hooks, `brain init --agents` / `brain uninstall`.
- Loopback settings UI, memory graph/explorer, work map with an eight-region
  overview (counts only for loaded projects/tasks).
- Task coordination: tasks, claims, fences, leases; `brain-coord/1`
  coordinator over loopback TLS; default-deny fact sharing.
- Opt-in conversation archive for Claude Code, Codex and Hermes visible text.
- `install.sh` source installer with `--dry-run` and `--uninstall`.
- Licence policy check with SPDX expression parsing; third-party notices.

### Security and upgrade notes
- Local UI data requires the per-window owner key; launcher arguments contain
  only a private browser-bootstrap path, never the key.
- Background workers use a persistent kernel lock. When upgrading an older
  installation, finish old workers and restart clients that could spawn the
  old executable before using the new queue worker.
- Hermes input sources must be regular files of this user and not writable by
  group/other; ordinary `0644` is accepted read-only. `0664` is refused with
  a permission diagnostic. Sources are never automatically chmodded.
- Shared fact payloads now reject reserved identity/authority keys recursively
  and without case distinctions, including incoming facts from older peers.
- Filtered memory search reports its bounded, non-exhaustive candidate window.

### Known limits
- Not tested as a distributed fleet; LAN binding unreleased.
- macOS and Windows unsupported.
- Archive capture is partial and global to the owner (no project ACL).
- No formal security verification or external audit.
