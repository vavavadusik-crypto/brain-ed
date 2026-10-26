# Security model (current development preview)

This describes intended boundaries and what is tested. It is not a formal
proof, and no external audit has been done.

## Trust boundary

- **Single user, single machine.** All state lives under `~/.brain` with
  owner-only permissions where the code enforces them (e.g. map and
  coordination databases: directory `0700`, file `0600`, owner and no-symlink
  checks). Any process running as the same OS user can read that data and is
  outside the boundary.
- **Loopback by default.** `brain ui` binds `127.0.0.1`. The coordinator
  (`brain net`) serves `brain-coord/1` over loopback TLS by default; tokens
  live in `0600` credential files or inherited descriptors, never in argv.
  An experimental, TLS-only LAN listener needs `--listen <private IP>
  --allow-lan` (wildcard, multicast, `255.255.255.255`, link-local and public
  addresses are refused; subnet broadcast/network addresses cannot be
  detected without the netmask). It has no rate limiting: unauthenticated
  LAN hosts can exhaust the 64 global connections, so an operator firewall
  is required. Shared CGNAT/overlay addresses additionally have their own
  explicit `--allow-cgnat` listener and `--allow-cgnat-dial` client options;
  the LAN flag does not admit them. Not production- or fleet-proven.
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

The archive stores visible user/assistant text from Claude Code, Codex and
Hermes plus a bounded journal of what the agent did — tool calls, tool
results, tool errors and visible reasoning summaries, each capped at 2048
characters (`JOURNAL_MAX`) — after `brain archive enable`. Credentials are
masked at write time (pattern-based, `MASK_REV = "mask:3"`), which is not a
guarantee: treat the archive as containing secrets and keep it `0600`. Hidden
or encrypted reasoning, system/instruction text and sidechains are never
stored. Imported tool traffic that would mislead search is quarantined, not
deleted. Coverage is best-effort and cannot be guaranteed complete; see
[docs/journal.md](journal.md) for the exact per-role shapes.

Agents reach this text only through `brain ask`, `archive search`/`read` and
the `memory_search`/`archive_search` MCP tools, where every archived row is
marked untrusted evidence and budgets bound how much reaches them.

## Network and downloads

Local memory and retrieval commands need no cloud connection. `brain model
install` downloads embedding weights only after explicit approval; `--offline`
forbids those downloads. `install.sh` downloads a selected release binary and
its checksum manifest over HTTPS, verifies the digest and installs only the
CLI. It does not run the downloaded binary, use sudo or configure agents.
The separate `packaging/install-source.sh` runs `cargo build --locked` and may
fetch locked crates. Checksums from the same release are not signatures.

Explicit `llm test` and `distill run` invoke the configured provider. HTTP
providers receive the selected text; subscription adapters invoke an already
authorized Claude Code or Codex CLI. An enabled distillation schedule can do
this later without another prompt. `--offline` is not a network sandbox and
does not block those explicit LLM calls. The archive remains untrusted during
distillation; validation of returned facts is not a proof against prompt
injection. Read [the adapter isolation limits](distiller.md#через-подписку).

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
