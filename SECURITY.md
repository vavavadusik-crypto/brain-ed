# Security policy

## Supported versions

Brain ED is an early preview. Only the latest `0.1.x` and the default branch
receive fixes. Only Linux is supported.

## Reporting a vulnerability

Do not disclose a vulnerability in a public issue. Once the repository is
public, use GitHub's private vulnerability-reporting form in its **Security**
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
