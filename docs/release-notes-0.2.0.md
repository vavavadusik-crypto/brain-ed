# Brain ED 0.2.0 — shared memory for your AI agents

**Brain ED is a local memory that lets AI coding agents retain and retrieve decisions, mistakes and project context across sessions.** The executable and package are called `brain`.

Claude Code, Codex and Hermes Agent can retrieve shared decisions, failed attempts and evidence instead of asking you again. You can search too: `brain ask` needs no chat model.

For a developer, Brain reduces repeated explanations; for companies, authorized assistants can retrieve selected project facts. Team mode is a preview that needs a controlled pilot.

## What is new

The [full changelog](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/CHANGELOG.md) records the details. The main changes since the earlier preview are:

- **Distill conversations:** extract decisions, errors, dead ends, findings and plans with evidence, session references and status history. Local/API and subscription providers, dry runs, budgets, rejection reasons, resumable progress and optional schedules.
- **Search earlier work:** bounded archive excerpts in memory search, RU/EN date phrases and distinct journal roles. Tool calls/results/errors and visible reasoning summaries are capped at 2048 characters; hidden reasoning is unavailable.
- **Share team facts:** manifests select recipients, spaces, classes and fields. Bounded generations carry permitted facts, task state and Git metadata. Answers expose source, age and coverage through ordinary questions and read-only MCP tools.
- **Connect deliberately:** TLS enrollment, CA fingerprint checks, roles, revocation and a company relay with end-to-end coordinator TLS. Direct TLS remains available; arbitrary public-NAT operation is unproven.
- **Inspect settings:** agent/privacy/team previews, settings history, keyboard/focus and reduced-motion improvements, offline graph exploration and read-only diagnostics. Views describe the loaded sample.
- **Import and recover:** Markdown/wiki-links and Graphify revisions, broader shared masking, backup/restore and opt-in backup timers. Local OpenTelemetry export sends no telemetry automatically.
- **Install more easily:** static Linux build tooling, SHA-256 download installer, walkthroughs and generated CLI reference. Workflow files do not prove an asset was published or tested.

## Install

Linux is the supported platform. Distribution targets are Linux x86_64 and aarch64; the ARM Linux executable is also intended for Android/Termux. It is not an APK. macOS comes before Windows in the portability plan; neither is supported in 0.2.0. Keep the Termux executable in its private HOME, outside shared Android storage. Termux needs Bash, curl and coreutils; optional archive tooling needs Python 3. Device-specific behavior still needs validation.

### Quick install (two commands)

```sh
curl -fsSLo install.sh https://raw.githubusercontent.com/vavavadusik-crypto/brain-ed/v0.2.0/install.sh
bash install.sh --version 0.2.0
```

The script verifies the SHA-256 of the downloaded binary before installing it into `~/.local/bin/brain`. Read it first if you prefer: it is short.

### Download installer: one install command

Download or check out `install.sh` from the reviewed `v0.2.0` source and inspect it. From that directory, the install command is:

```sh
bash ./install.sh --version 0.2.0
```

The script downloads the matching `brain-0.2.0-<target>` executable and `SHA256SUMS` over HTTPS, selects its exact checksum, verifies SHA-256 and installs `~/.local/bin/brain`. It never executes the downloaded payload during installation. It uses no sudo, installs no model or service and does not connect agents or enable archive collection. The checksum checks download integrity; it is not an independent publisher signature.

Preview without network or writes: `bash ./install.sh --version 0.2.0 --dry-run`. Add `export PATH="$HOME/.local/bin:$PATH"` to your shell profile. `--uninstall` removes the CLI and preserves Brain data.

Download installation requires matching 0.2.0 assets and a combined SHA256SUMS on the release. See [artifact requirements](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/build-release.md).

### Manual binary download and checksum

Download `SHA256SUMS` and the executable matching your architecture from the release assets. For x86_64, in a directory containing those two files:

```sh
asset=brain-0.2.0-x86_64-unknown-linux-musl
awk -v name="$asset" '$2 == name {print}' SHA256SUMS > selected.sha256
# Require exactly one matching entry before checking or installing.
test "$(wc -l < selected.sha256)" -eq 1 &&
  sha256sum --check selected.sha256 &&
  install -Dm755 "$asset" "$HOME/.local/bin/brain"
```

ARM64: set `asset=brain-0.2.0-aarch64-unknown-linux-musl`. Never install after a failed check. Assets are unsigned.

### Build from source

Use the `v0.2.0` source, the Rust version pinned in `rust-toolchain.toml`, C/C++ build tools and the locked dependencies:

```sh
git clone --branch v0.2.0 https://github.com/vavavadusik-crypto/brain-ed.git
cd brain-ed
cargo build --locked --release -p brain-cli
mkdir -p "$HOME/.local/bin"
cp "${CARGO_TARGET_DIR:-target}/release/brain" "$HOME/.local/bin/brain"
export PATH="$HOME/.local/bin:$PATH"
```

This is a native host build. See [installation](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/INSTALL.md) for requirements. No model weights are bundled.

## Your first five minutes

Five minutes excludes compilation and model downloads; it is a setup target.

```sh
brain init
brain init --agents --dry-run
brain init --agents
brain remember "Build uses cargo --locked" --kind decision --tag demo
brain --offline ask "What did we decide about the build?" --no-archive
brain ui
```

`init` creates or migrates the database. Review the agent preview before confirming changes; conflicts can be skipped. Reconnect clients after integration changes. `remember` stores a decision; `ask` retrieves evidence without a cloud key. For an exact check use `brain --offline recall "cargo locked"`. `ui` opens or prints the loopback settings URL; open it manually on Termux if needed. `brain doctor` reports configured integrations and observed capture. Configuration alone does not prove a client handshake. See [agent setup](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/INSTALL.md#connecting-agents-claude-code-codex-cursor) and [client profiles](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/client-profiles.md).

Optional semantic models require explicit `brain model install`; weights are not bundled. `--offline` blocks model downloads, not explicit LLM/remote requests.

## What you can do with it

### Keep agents aligned

Save decisions with reasons, errors with attempted fixes and plans with status. Connected assistants can retrieve them through MCP with provenance. Spaces retain explicit boundaries; correctness and agent use of memory still need review.

### Search the session journal

Archive collection is a separate opt-in. Review its sources and privacy settings before enabling it:

```sh
brain archive enable --since-days 7
brain archive collect
brain archive status
brain archive search "build failure" --limit 5
```

Collection reads supported local session files, masks recognized secrets and stores a private searchable archive. `ask` and MCP `memory_search` can include bounded archive evidence; use `--no-archive` in the CLI to exclude it. Date phrases and role filters help narrow earlier work. Coverage depends on available client files and supported formats. Inspect status rather than assuming all sessions were captured. See [journal boundaries](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/journal.md) and [archive guide](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/session-archive.md).

### Distill decisions and mistakes

Distillation reads the stored masked archive and writes validated structured facts into the memory graph. It is optional and requires a separately configured provider. Choose one route:

- **Stay on the machine:** run your own OpenAI-compatible model server on loopback, then register it as shown below. Brain does not install or start that server.
- **Use an existing Claude Code subscription:** register the built-in CLI adapter. The CLI must already be installed and authorized; model access and limits depend on the subscription. The Claude adapter has tools disabled. A Codex subscription adapter is available too, with its own documented constraints.
- **Use an API:** configure an OpenAI-compatible HTTPS endpoint and the name of an environment variable holding its key. Selected archive text goes to that provider under its terms. A separate API bill may apply.

```sh
brain llm add local --base-url http://127.0.0.1:8080 --model local-model --context-tokens 16384
brain distill run --provider local --dry-run --max-sessions 5
# After reviewing the plan, call the chosen local provider:
brain distill run --provider local --max-sessions 5
```

Alternatively, for an already authorized Claude Code CLI:

```sh
brain llm add subscription --command claude --model sonnet --effort medium --context-tokens 128000 --timeout-seconds 900
brain distill run --provider subscription --dry-run --max-sessions 5
brain distill run --provider subscription --max-sessions 5
```

Dry runs call no provider and write no facts. Real runs report rejections and retain successful progress. Validation checks evidence consistency, not truth. See [walkthrough](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/distill-guide.md) and [provider/API reference](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/distiller.en.md).

### Try company/team memory

Start with disposable synthetic data from a source checkout:

```sh
tools/team-demo.sh
tools/team-demo.sh --via-relay
```

These demos use separate temporary homes and real local processes; they demonstrate the protocol, not a physical fleet. Follow [team setup](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/team-mode.md), [publication and manager questions](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/team-mode-facts.md) and [relay operations](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/relay.md) before a pilot.

Source manifests explicitly select projects, spaces, recipients and classes/fields. A manager can ask an assistant “Which errors remain open?” or “What decisions were made?” Read-only `team_ask` queries specified configured machines; `team_manifest` and `team_peers` show policy and freshness. They cannot enable publication.

Answers show source, time, age and coverage; a missing machine is a gap. Permitted summaries, task state and recent Git metadata can be shared. The team schema never transmits raw chats, tool output, files, paths, diffs or Git author/email. Approved summaries can still contain sensitive prose. There is no screen or arbitrary-file surveillance.

## Privacy and security boundaries

Storage and retrieval are local by default, without automatic telemetry. Explicit cloud/API or subscription distillation sends selected masked text externally; choose a loopback provider for local processing.

Recognized credentials are masked on ingestion and supported imports. Unknown secret formats and ordinary private information may remain. Do not treat masking as permission to publish an archive. Archived text, model responses and team facts are **untrusted data**, never system instructions. Provenance supports checking an assertion; it does not certify it.

The private archive belongs to its local owner and has no project ACL. A team grant must not be interpreted as access control for raw archive text. TLS and slice digests protect the configured transport and integrity; slices do not yet have Ed25519 author signatures. Credentials are not Biscuit tokens. A company relay transports nested TLS and does not store stream content, but the operator still manages its host, availability and logs.

## Known limits of 0.2.0

- Linux-first preview. ARM/Termux targets do not establish compatibility with every phone. macOS/Windows need security, filesystem, scheduling, packaging and CI work; changing a build target alone is insufficient.
- The [release audit](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/release-audit-0.2.md) reports local synthetic tests and a checked source export. New musl artifacts, remote GitHub CI and publication were outside that run. README download tests substituted local data. These are separate release checks, not proof from the source tests.
- No formal security proof or independent security audit, no enterprise/fleet readiness claim, and no public-NAT or long-running multi-machine reliability proof.
- Capture is partial, role records are bounded, retrieval and graph views are bounded, and history can be unknown. Shared slices have independent source watermarks rather than a simultaneous global snapshot. Current slices contain at most 24 entities; recent Git metadata is limited to eight commits.
- Team publication excludes raw data and has no raw-text fallback. Revoking a policy stops future authorized delivery; it cannot erase information a recipient already copied. An assistant label does not establish the identity of the human behind it.
- Timers use Linux systemd user services. Their presence does not prove they ran successfully; Termux needs an appropriate manual workflow where systemd is unavailable. LLM quality, context capacity and subscription limits affect distillation.
- Release artifacts are unsigned and independent reproducible builds have not been established. During CLI upgrades, reconnect clients still running an older MCP process. Keep backups; `0.x` formats and interfaces may change.

## What comes next

The next milestone focuses on a controlled company pilot, repeatable GitHub CI and release checks, easier fleet installation planning, and better local/API distillation workflows. Medium-term work covers macOS before Windows, stronger slice signatures and constrained delegation, source-machine search, and a manager web/mobile panel. P2P/NAT traversal and broader deployment validation follow evidence from pilots. These are intentions, not shipped features or delivery dates: see the [roadmap](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/ROADMAP.md).

## Documentation

- [Documentation index](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/README.md)
- [Team setup](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/team-mode.md) and [team facts / assistant questions](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/team-mode-facts.md)
- [Distillation walkthrough](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/distill-guide.md)
- [Company relay](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/relay.md)
- [Agent integrations](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/INSTALL.md#connecting-agents-claude-code-codex-cursor)
- [CLI reference](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/cli-reference.md)
- [MCP, hooks and reconnecting clients](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/mcp-and-hooks.md)
- [Security boundaries](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/SECURITY.md) and [development plan](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/ROADMAP.md)
