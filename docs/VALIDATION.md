# Validation of the Linux previews

## 0.1.2 source preparation — local checks observed by the maintainer

The 0.1.2 source preparation raises the workspace version and adds the Linux
x86_64 bundle module (`tools/package-linux.py`, `packaging/install-linux.sh`,
`tests/test_linux_bundle.py`, the manual build-only
`.github/workflows/linux-bundle.yml`; see
[linux-download.md](linux-download.md)). At the time of writing this source
preparation was not published when these checks were recorded. The maintainer observed these local checks
(phase 77) on the combined 0.1.2 source; this file records the reported
results and does not reproduce them:

| Check | Observed result |
|---|---|
| All 13 local gates | passed |
| Rust workspace, all features | 473 passed |
| Rust workspace, no default features | 468 passed |
| Clippy, both feature sets, `-D warnings` | passed |
| `cargo fmt --check` | passed |
| Node suites | 2 passed |
| Licence check (247 crates) and parser self-test | passed |
| Release build | passed |
| Python suites | 116 passed |
| Collector on tmpfs | 29 passed |
| Linux bundle suite (31 tests) against the actual 0.1.2 binary, normal user uid 1000 | 30 passed, 1 skipped (the root-refusal test runs only as root) |

The bundle suite covers: SHA256SUMS with a NUL byte refused
before the binary runs; existing outputs and symlinks never overwritten (O1);
strict repository URLs and scanned generated metadata (S1); extraction with
real GNU tar; two desktop and two menu launchers in a synthetic HOME; and
uninstall keeping the database and settings.

Source secret scan: Gitleaks reported 26 findings. All are byte-identical to
synthetic test fixtures and API identifiers already in the public a9c7134
source, and were reviewed; no actual secret was found in the checked source.
This is not a formal or global security audit.

Not verified: the bundle workflow on GitHub (it has no recorded CI run), the
bundle's ABI and portability beyond its build system, any CI run of this
preparation, and any provenance. Known workflow issue: with the pinned
`actions/checkout`, disabling sparse checkout and then unsetting
`extensions.worktreeConfig` leaves `.git/config.worktree` behind. This was
confirmed locally with git 2.53, and the packager refuses that source as
intended. The workflow now builds, packages and tests from an ordinary clone
of that checked-out commit; a local regression passed, but the real runner
has not been tested. Other git versions were not checked. Bundle checksums
come from the same download, are not signed and
do not authenticate it. Brain ED remains a standalone preview: no enterprise,
complete-capture or fleet claims. The 0.1.1 results below were measured on
0.1.1 and are kept as dated historical evidence; they were not rerun for
0.1.2.

## 0.1.1 local release checks — 2026-10-10

The combined LAN, remote MCP, memory explorer and installer candidate was
checked as Linux x86_64 user 1000 with Rust 1.97.1, locked dependencies and
offline Cargo. All 13 local checks passed. The 148 source-file hashes stayed
unchanged throughout the suite; this report was updated afterwards.

| Check | Observed result |
|---|---|
| Rust workspace, all features, default parallel tests | 473 passed, 0 failed |
| Rust workspace, no default features | 468 passed, 0 failed |
| Clippy both feature configurations, all targets | Passed with `-D warnings` |
| Formatting; Node map and memory graph suites | Passed |
| Python archive, collector, backfill, bridge and real-binary E2E | 116 passed, no skips |
| Collector on tmpfs | 29 passed |
| Release build; ShellCheck; licence checks | Passed; 247 dependency packages checked |
| Actual source installer in a synthetic HOME with spaces | Two desktop and two menu entries; installed bytes match the build |
| Install/uninstall dry runs; uninstall | No dry-run changes; synthetic SQLite database bytes preserved |

The Rust totals overlap. Remote MCP tests include real TLS sockets, separate
synthetic stores, fixed-tool authorization, malformed/error replies, project
grammar, duplicate configuration keys and the whole 16 KiB reply budget.
LAN address tests exercised alternate loopback IPv4 and IPv6; they do not
prove connectivity between physical computers. The memory explorer was also
checked in a browser using synthetic data at wide and 390 x 844 viewports,
including the 2D fallback. Private history was not used in that browser test.

Independent Opus review of the remote adapter and root review/reproductions
found issues with key collisions, trimming cost and misleading server errors.
The fixes and regressions are included. Final malformed-envelope and project
grammar changes were reviewed by root and passed the combined suite.
This is a bounded review of the changed first-party paths, not an external
certification or a line-by-line audit of all dependencies.

The first source scan produced 26 reviewed matches: synthetic authorization
fixtures, synthetic UI tokens, token-estimator identifiers and SQLite API
macros. No actual provider credential or personal data file was found in
that source snapshot. Final Git history and archive scans are separate
publication gates. A successful scan cannot detect every possible secret.

The source installer is still Linux-only and requires Rust and a C build
toolchain. No prebuilt binary, physical fleet validation, complete native
capture or enterprise access-control claim is made. `--offline` controls
model downloads; explicitly enabled remote tools still make TLS calls.

## 0.1.0 historical local release checks

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
- The original 0.1.0 brain-shaped screenshot demonstrates the bounded project
  map. In 0.1.1 the memory explorer also projects its loaded selection by
  stored kind; neither view displays every record at once.
- The UI currently permits inline scripts in its CSP. Schema/path hardening for
  owner-supplied import sources and peak memory use for large JSON imports remain
  follow-up work. Do not treat the preview as an enterprise isolation boundary.
- Upgrading a pre-flock worker requires excluding mixed worker generations.
  Reconnect long-lived MCP clients to load the new executable and tools.

To reproduce the suites, use the commands in `.github/workflows/ci.yml`.
Models are not downloaded by these release checks.
