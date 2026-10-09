# Validation of the 0.1.0 Linux preview

Recorded on 2026-10-09 on Linux x86_64 as an ordinary user, using Rust 1.97.1.
These are local release checks. GitHub CI is a separate result shown in the
repository Actions tab. This report is not a security certification.

| Check | Observed result |
|---|---|
| Rust workspace, all features, default parallel tests | 442 passed, 0 failed |
| Rust workspace, no default features | 437 passed, 0 failed |
| Clippy, all targets, both feature configurations | Passed with `-D warnings` |
| Rust formatting | Passed |
| JavaScript map and graph suites | Both passed in Node |
| Python archive, collector, backfill, bridge and real-binary E2E | 116 passed, no skips |
| Collector on tmpfs | 29 passed |
| Source installer in a synthetic HOME containing spaces | Passed: two desktop and two menu entries, matching installed binary bytes |
| Installer dry runs and uninstall | No synthetic HOME changes on dry run; database bytes preserved on uninstall |
| Shell syntax and ShellCheck | Passed |
| SPDX licence policy | 247 packages checked; parser self-tests passed |

The feature configurations share many tests; the two Rust totals are not
879 distinct tests. Python tests exercise adapters using synthetic client
fixtures and actual CLI/MCP processes. They do not prove every installed
third-party application captures every ongoing conversation.

## Security and privacy review

First-party review covered memory/archive scope, TLS and sharing authorization,
input bounds, read-only SQLite access, private file modes, UI owner keys,
installer paths and untrusted-text rendering. Regression checks include
uppercase SQLite views, concurrent worker ownership, descriptor inheritance,
reserved nested authority keys and retrieval filters before output budgets.

Gitleaks 8.30.1 scans of the source, fresh Git history and source archive were
reviewed before publication. Matches were synthetic test values and API or
token-estimation identifiers; no actual provider credential was found. The
source does not contain a personal database, session archive, provider profile,
model weights or the original private Git history. Example images use synthetic
data. This does not guarantee that every possible secret pattern is detectable.

The automated formal deep-scan worker could not start in the available
filesystem permission environment. Independent code review, dependency advisory
comparison and regression tests are separate checks, not a formal scan result.
Vendored C/JavaScript and all upstream dependency source were not audited line
by line. Two dependencies have unmaintained advisory notices; the checked lock
snapshot contained no confirmed affected security advisory after the rustls
upgrade to 0.23.45.

## Remaining verification limits

- TLS tests use real sockets and isolated stores on one machine. No LAN or
  physical fleet deployment is claimed.
- HTTP owner-key and anonymous refusal checks passed. The private `file://`
  browser-bootstrap end-to-end check was unavailable in the verification
  browser, whose policy refuses local files. It was not bypassed or reported
  as a browser pass. Launcher quoting and private bootstrap file modes were
  checked separately.
- The brain-shaped screenshot demonstrates the bounded project map, not a
  complete brain-shaped projection of all personal memory records.
- The UI currently permits inline scripts in its CSP. Schema/path hardening for
  owner-supplied import sources and peak memory use for large JSON imports remain
  follow-up work. Do not treat the preview as an enterprise isolation boundary.
- Upgrading a pre-flock worker requires excluding mixed worker generations.
  Reconnect long-lived MCP clients to load the new executable and tools.

To reproduce the suites, use the commands in `.github/workflows/ci.yml`.
Models are not downloaded by these release checks.
