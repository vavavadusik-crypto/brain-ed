# Changelog

All notable changes are listed here. Versions follow semantic versioning;
`0.x` releases may change formats and interfaces.

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
