# Brain ED 0.1.3: verification and limits

This record covers local checks of the 0.1.3 release candidate. Matching
published source and download assets are identified on the GitHub release
page. A bundle installs its own version; an older bundle does not install
newer source. CI results are available separately on GitHub Actions.

## Local checks

Checks ran as a normal Linux user (uid 1000), with temporary profiles for
installation and archive tests. Personal profiles and databases were not used
as test fixtures.

| Check | Observed result |
|---|---|
| 13 local gates | passed |
| Rust workspace, all features | 494 passed |
| Rust workspace, no default features | 489 passed |
| Clippy, both feature sets, `-D warnings` | passed |
| Formatting and both JavaScript suites | passed |
| Licence inventory; parser self-test | 247 crates; 22 checks passed |
| Versioned release build | `brain 0.1.3` |
| Python suites | 116 passed |
| Collector on tmpfs | 29 passed |
| Linux bundle suite against the 0.1.3 binary | 31 passed, 1 skipped |

The skipped bundle test requires uid 0. The normal-user run did not claim
that test passed. Bundle tests exercise malformed archives, bounded admission,
unsafe Git configuration, ELF validation, tampering, installation, dry runs,
and data-preserving removal in synthetic profiles.

## Native-home and uninstall behaviour

A separate normal-user run traced file operations on the release binary:

- Four CLI cases (doctor, agent-install dry run, uninstall dry run, uninstall)
  did not access the six skipped Claude/Codex default configuration files,
  their blocked backups, or the override paths. Override values did not
  appear in command output.
- A real permission denial (`EACCES`) and manual edits remained conflicts;
  the affected files, manifest, backups and shared binary were preserved.
- A later uninstall after a partial run completed without rewriting files
  already restored, restored the remaining originals, and cleaned up the
  manifest and backups. Desktop ownership still retains a shared binary.
- Sixteen independent negative CLI cases with file-operation tracing kept all
  synthetic files unchanged and did not open forbidden targets. They cover a
  valid-checksum unrelated path, backup traversal and absolute paths, duplicate
  entries, invalid versions and hashes, oversize or permissive manifests and
  symlinks in the manifest, Brain home, backups and client paths.

## Review and privacy boundary

The changed client-home, doctor and installer code, plus connected main,
private-filesystem and desktop paths, received an independent read-only
review. Known limits below remain; this is not a formal or global audit.
A scoped source scan found 26 matches, identical to previously reviewed
public fixtures and API identifiers, with no new matches in that scan.
Personal histories, credentials, runtime databases and private configuration
are excluded from the source release. The packaging workflow performs
separate checks on the exact source commit and built bundle.

## Remaining limits

- Linux x86_64 only, dynamically linked. The manual Ubuntu 24.04 build uses
  glibc 2.39. Older glibc, other distributions and other platforms are unproven.
- No physical fleet deployment, enterprise access-control certification,
  complete conversation capture, or reconnection of already-running desktop
  clients is established by these checks.
- Relocated client homes are detected and skipped, not followed or migrated.
  Matching uses the current spelling of `HOME`; an alias or a changed home
  can put old paths outside the allowlist and refuse the whole uninstall.
  Hermes has no automatic
  global MCP/hook installation in this preview.
- `misplaced_claude_mcp_entry` keeps its legacy boolean type: `false` can mean
  not checked. Use the additive client configuration state alongside it.
- Uninstall admits only the current default client paths, a bounded valid
  manifest and owner-only regular backups. Do not use an untrusted or shared
  writable Brain home. Filesystem changes between parent-directory checks and
  writes remain a limitation. Install-side configuration symlinks can still
  be replaced with regular files; uninstall skips client-path symlinks.
  An existing permissive or symlinked Brain home refuses uninstall even if
  no agent-install manifest is present. The command does not change its rights.
- Downloads have checksums, not signed maintainer attestations. There is no
  signed build-provenance claim or formal DeepScan certification.

`VALIDATION.md` remains the historical 0.1.2 verification record.
