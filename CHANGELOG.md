# Changelog

All notable changes are listed here. Versions follow semantic versioning;
`0.x` releases may change formats and interfaces.

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
