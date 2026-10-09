# Security model (0.1.0 early preview)

This describes intended boundaries and what is tested. It is not a formal
proof, and no external audit has been done.

## Trust boundary

- **Single user, single machine.** All state lives under `~/.brain` with
  owner-only permissions where the code enforces them (e.g. map and
  coordination databases: directory `0700`, file `0600`, owner and no-symlink
  checks). Any process running as the same OS user can read that data and is
  outside the boundary.
- **Loopback only.** `brain ui` binds `127.0.0.1`. The coordinator
  (`brain net`) serves `brain-coord/1` over loopback TLS; tokens live in
  `0600` credential files or inherited descriptors, never in argv. LAN binding
  is not released.
- **MCP over stdio.** `brain serve` has no network listener.

## Data classes

| Data | Scope | Notes |
|---|---|---|
| Memory records | per space, per agent ownership | `memory list/show/delete/export` act on this agent's records |
| Coordination | per space, tasks/claims/leases | advisory: agents that never ask are not stopped |
| Shared facts | default deny | host enables a space and issues an explicit `memory:read:<space>` grant |
| Conversation archive | **global to the owner** | not project-scoped, **no project ACL**; results carry `untrusted` provenance |
| Activity | agent-declared hook metadata | no prompt text, file names or working directories shown; no screen/file surveillance |

## Untrusted content

Archive text, imported graphs, hook payloads and memory contents are treated
as data. Archive search/read results are marked untrusted and bounded in size;
agents must cite them, never follow them as instructions. Retrieval budgets
bound how much text reaches an agent.

## Capture limits

The archive stores only visible, allowed user/assistant text from Claude Code,
Codex and Hermes after `brain archive enable`. Thinking, tool calls and tool
output are dropped. Coverage is best-effort and cannot be guaranteed complete.

## Network and downloads

Ordinary commands never download. `brain model install` downloads embedding
weights only after explicit approval; `--offline` forbids it. `install.sh`
runs `cargo build --locked` (crates.io fetch of locked versions), uses no sudo
and uploads nothing.

## Local UI owner access

Served HTML contains no owner key. Data APIs and `brain-data.json` require
`x-brain-token`; mutating requests also reject foreign origins. The launcher
writes an owner record and a browser bootstrap HTML file beneath
`~/.brain/ui` (`0700` directory, `0600` files). Browser command-line arguments
contain only the bootstrap file path. The file passes the key in a URL
fragment; the page moves it to per-tab session storage and clears the fragment.
The key is still a bearer credential readable by the same OS user. Do not
share owner records, bootstrap files, authenticated browser state or key URLs.

On Linux, reopening an existing window checks the owner file, PID, generation
and listening socket before recovering its key. Stale, permissive, symlinked
or mismatched owner records fail closed. A browser sandbox that cannot read
`~/.brain/ui` may require manually opening the private owner URL; never expose
the UI on a public interface to work around that restriction.

Fresh default data directories/files use owner-only modes. Existing explicit
database parents are not silently chmodded. Back up existing data and check
its permissions separately; this does not promise encryption at rest or
automatic redaction of secrets typed into visible conversation text.

## Validation and remaining limits

The release checks include synthetic owner-access, TLS authentication and
expired-certificate rejection, space isolation, transcript filtering, and
installer tests. Certificates in tests use fixed historical dates compatible
with OpenSSL 3.0 and 3.5. Independent reviews and secret/dependency scans do
not constitute a formal proof or a complete audit of third-party code.

The preview retains known limits: a global owner archive, advisory task
coordination, bounded client capture, no enterprise project ACL, no proven
physical fleet rollout, and agent-declared activity rather than operating
system surveillance. Avoid importing untrusted large graph/database files;
some owner-only import and graph paths have no total input-size cap. See the
README capability table before choosing this preview for a deployment.
