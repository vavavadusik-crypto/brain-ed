# Brain ED 0.1.4: verification and limits

This record covers local checks of the 0.1.4 release candidate. Exact published
source and matching download assets are identified on the GitHub release page;
CI results are available separately on GitHub Actions. A bundle installs its own
version. None of these checks establishes complete agent capture.

## Local checks

Checks ran as an ordinary Linux user (uid 1000), using synthetic profiles and
private temporary fixtures. Personal client profiles and memory databases were
not test inputs. The production source stayed unchanged during the full run.

| Check | Observed result |
|---|---|
| Complete local validation pipeline | 13 gates passed |
| Rust workspace, all features | 549 passed, 1 helper ignored by the normal harness |
| Rust workspace, no default features | 544 passed, 1 helper ignored by the normal harness |
| Clippy, both feature sets, `-D warnings` | passed |
| Formatting and both JavaScript suites | passed |
| Licence inventory and expression parser | 247 packages; 22 cases passed |
| Release build | `brain 0.1.4` |
| Python suites | 116 passed |
| Collector on tmpfs | 29 passed |
| Linux bundle suite against the release binary | 31 passed, 1 root-only test skipped |

The skipped bundle test requires uid 0; it was not claimed as a pass. The
other bundle tests exercised packaging, unsafe Git configuration, ELF checks,
archive bounds, tampering and synthetic installation/removal.

The ignored Rust test is a child-process helper, invoked explicitly by its
running-image parent test. It is not counted as a normal-harness pass.
No serial test-run override was used. Socket and TLS tests ran on the host:
the restricted shell sandbox refused socket creation before application code.
Earlier sandbox failures were retained and are not reported as passing runs.

## Client-profile and HOME boundaries

The registry and explicit Codex MCP integration received scoped independent
read-only reviews, followed by root inspection and executed regression checks.
They cover bounded state, concurrent changes, durable intent/recovery, default
home identity, configuration size, and copying the actual running executable.
This is not a formal or repository-wide security certification.

Separate CLI checks on the release binary established:

- 119 negative cases: seven unsafe HOME forms across seventeen integration,
  removal, shortcut and registry commands. Each refused with a fixed path-free
  message and no fixture writes, including no database creation by
  `init --agents`.
- Twenty positive and refusal checks in a Unicode HOME: dry runs, the exact
  running binary copied to the integration path, metadata-only diagnostics,
  real permission denial, configuration drift, exact-byte restoration and
  retained integration binaries.
- A real installed 0.1.3 binary's default install/uninstall preserved the new
  registered Codex integration and its synthetic database. Shortcut installation
  and removal also preserved them.
- The actual source installer created two desktop and two application-menu
  entries. Install/uninstall dry runs changed nothing; removal preserved the
  synthetic database and settings.

These checks do not reconnect a desktop application or establish a client
handshake. Registration alone never configures a client or reads its content.
Only an explicit registered Codex opt-in edits `config.toml`; Claude Code and
Hermes registrations have no automatic integration.

## Hook contract

Twelve process contract tests passed, covering bounded valid, malformed and
oversized input; idle pipes/sockets; private state; symlinks/FIFOs; and unsafe
HOME. Additional real Python pipe, Unix socket-pair and Node.js `spawn` launchers
on the release binary each produced one synthetic journal record, exactly `{}`
plus newline on stdout, and exit 0. Node's nominal `stdio: 'pipe'` was observed
to be a socket. These are launcher tests, not observed callbacks from a real
Claude Code or Codex session.

Linux pipes/files use a separately reopened non-blocking descriptor; stream
sockets use per-call `MSG_DONTWAIT` without changing shared descriptor flags.
Input retention, draining and deadlines remain bounded. Journaling is
best-effort: oversized input, unsafe state, lock contention and I/O failure
can drop events. Filesystem latency is outside the polling deadline.
See [the full hook contract](mcp-and-hooks.md#hook-process-contract).

## Privacy and release boundary

The scoped source scan reported 27 matches: 26 byte-identical public fixtures
or API identifiers and one explicit synthetic GitHub-token fixture in a new
negative test. The matching source contexts were reviewed; no actual secret
was found in that scope. Personal histories, auth, runtime databases, local
verification outputs and private configuration are excluded from source.
Exact history, archive and download checks are separate release gates.
No formal DeepScan or external-audit certification is claimed.

## Remaining limits

- Linux x86_64 only. The manual Ubuntu 24.04 bundle is dynamically linked;
  its manifest records the highest required glibc symbol version. Earlier
  bundles used glibc 2.39. Other distributions, older glibc and non-Linux
  platforms remain unproven.
- TLS tests use separate synthetic installations and ten local clients,
  including an opt-in alternate loopback IP. No physical LAN/fleet deployment,
  enterprise ACL certification or complete conversation capture is established.
- Client markers and Brain-owned integration records show configuration,
  not client execution. Already-running desktop MCP connections are not
  automatically reconnected.
- The registry lock coordinates cooperating Brain commands. Same-user
  directory replacement and filesystem changes between checks and writes
  remain limitations; Brain is not a sandbox. See
  [client profiles](client-profiles.md#limits-of-these-checks).
- Registered Codex edits replace the configuration inode with a private 0600
  file. ACLs and extended attributes are not preserved. Drift refuses recovery;
  original configuration text is not stored. Integration binaries are retained,
  never silently upgraded or automatically deleted. Follow the explicit
  [recovery procedure](registered-codex-mcp.md#manual-recovery-fail-closed-states).
- Default integration keeps its conservative manifest and relocated-home
  refusals. The new pure HOME guard applies to integration and shortcut
  commands, not to every arbitrary command or every raw XDG environment value.
- Downloads have checksums, not signed maintainer attestations or signed build
  provenance. The packaging manifest records an association with source, not
  a proof of how the binary was built.

`VALIDATION.md` and earlier version verification files remain historical records.
