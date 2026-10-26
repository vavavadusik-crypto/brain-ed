# Security policy

## Supported versions

Brain ED is an early preview. Only the latest released preview and the default branch
receive fixes. Only Linux is supported.

## Reporting a vulnerability

Do not disclose a vulnerability in a public issue. Once the repository is
public, use [GitHub's private vulnerability-reporting form](https://github.com/vavavadusik-crypto/brain-ed/security/advisories/new) in its **Security**
tab. If that form is unavailable, contact the maintainer through their GitHub
profile without including exploit details, secrets or a memory database in the
first message.

Include the Brain ED version or commit, OS, affected command or MCP tool,
impact, and a minimal reproduction with synthetic data. Remove API keys,
credentials, personal paths and real memory or archive contents from logs.

## Scope

In scope: cross-space or cross-agent data access; archive or memory text
reaching agent instructions; coordinator/TLS authentication or authorization
bypass; sharing without an explicit grant; unsafe installer or launcher
changes; secret disclosure; unbounded parser/resource use; vulnerable
dependencies.

Out of scope: answer quality without security impact; attacks that already
run code as the same OS user (outside the isolation boundary); exposing the
loopback UI or coordinator through a proxy or LAN against the documentation.

The threat model and data boundaries are in [docs/SECURITY.md](docs/SECURITY.md).

## Threat model in brief

- Local state belongs to one OS account. Another process running as that user
  can read it; Brain does not provide isolation from that process or encryption
  at rest. The archive is global to its owner and has no project ACL.
- Archive and imported memory text are untrusted evidence. Pattern masking
  can miss secrets; do not treat retrieved text as instructions or assume an
  archive is safe to publish.
- MCP uses stdio; the UI and coordinator use loopback by default. LAN transport,
  archive collection, model downloads and LLM calls need explicit setup/actions.
  Distillation sends selected text to the chosen provider; subscription clients
  have their own data policies and tool/isolation limits. `--offline` prevents
  embedding downloads, but does not disable an explicitly requested LLM call.
