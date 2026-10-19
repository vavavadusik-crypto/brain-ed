# Brain MCP for a registered Codex home (R1)

`brain clients configure --label L` adds one Brain MCP server entry to the
`config.toml` of a Codex home that you registered explicitly with `brain
clients register`. `brain clients unconfigure --label L` removes it again.

This is the whole scope:

- only `config.toml`;
- only a marked block, shown below.

It does not touch hooks, `AGENTS.md`, profile files, trust state, auth, `.env`
or history. It does not reconnect running Codex sessions, prove a handshake or
capture anything. A configured entry is not a working connection until Codex
loads it.

```console
brain clients configure   --label work-codex --dry-run   # plan only, writes nothing
brain clients configure   --label work-codex --yes
brain clients unconfigure --label work-codex --dry-run
brain clients unconfigure --label work-codex --yes
```

Exactly one of `--dry-run` and `--yes` is required. A dry run takes no lock and
creates no directory, file, intent or binary. The output shows:

- the label;
- the file name (`config.toml`);
- the action;
- the block Brain would insert.

It never shows lines from your configuration, its secrets or the registered
path.

## Requirement: an absolute, private HOME

The Brain home is `$HOME/.brain` (there is no separate `BRAIN_HOME`
setting). Configure and unconfigure, including `--dry-run`, refuse with a
fixed message and change nothing when `HOME`, or the Brain home derived from
it, is:

- unset or empty (it would become `.` or a relative path);
- relative;
- not UTF-8;
- containing a control character, including DEL (`0x7f`).

This check runs before any lock, registry or state read, binary copy or
`config.toml` access, and the message never quotes the path. A relative
command in the MCP block would be resolved against Codex's working directory
instead of the copied binary, and a control character cannot be written into
valid TOML. Paths are not canonicalized: an existing `HOME` alias such as
`/home -> /var/home` keeps working. Use a `HOME` that is a private directory
owned by you; the Brain home itself must be owner-only (0700).

## The inserted block

```toml
# BRAIN_REGISTERED_MCP_BEGIN
[mcp_servers.brain]
command = "<HOME>/.brain/integrations/bin/brain"
args = ["serve"]
# BRAIN_REGISTERED_MCP_END
```

- **Escaping:** the command is TOML-escaped; no shell is involved.
- **The one path it contains:** `command` is the absolute path of your own
  integration binary under your Brain home, so this block (and only this
  block) legitimately contains your `HOME` path. It holds no secrets. Other
  places Brain writes avoid paths, but this is not a claim that no path
  appears anywhere.
- **Placement:** the block is appended after your content. Your bytes
  (comments, other `[mcp_servers.*]`, `[[hooks.*]]` and everything else) are
  kept exactly as they are.
- **Brain never writes:** `[hooks.brain]`, `state`, `trusted_hash`, `env` or
  any auth value.

## What is refused (and nothing changes)

**The registration.** Configure and unconfigure refuse when:

- the label is not a valid Codex registration;
- the registered directory no longer passes its check, for example a changed
  identity or a symlink;
- the registration is the default `~/.codex`, by path or by inode. If
  `~/.codex` is a symlink, the directory it resolves to counts too: only its
  metadata is read, never its contents. A `~/.codex` that exists but cannot
  be resolved, such as a broken symlink, is also refused. That directory
  belongs to `brain init --agents`.

**`config.toml` itself.** It is refused when it is:

- a symlink, FIFO, directory or other non-regular file;
- not owned by you, or readable or writable by group or others;
- larger than 1 MiB, or larger than 1 MiB once the Brain block is added
  (checked before anything is written, so `unconfigure` can always read the
  file back);
- not UTF-8;
- already holding Brain markers (from this feature or from the default
  integration);
- already defining `[mcp_servers.brain]`;
- using a construct Brain cannot delimit safely, such as triple-quoted
  strings.

User hook arrays are accepted only in the plain form `[[hooks]]` or
`[[hooks.<bare-key>]]` written alone on a line. Quoted keys such as
`[[hooks."PreToolUse"]]`, or a comment after the header, are refused; this is
conservative, because that text handling is new.

**Brain's own state.** It is refused when an integration record or pending
state under `~/.brain/integrations/` is unsafe, oversized, corrupt or belongs
to another identity.

**The integration binary.** It is refused when an integration binary already
exists with a different hash: there is no automatic upgrade. It is also
refused when the running executable is not a regular x86-64 ELF file of at
most 128 MiB.

## Unconfigure

Unconfigure removes exactly the inserted block and writes the file back only
if the result has the recorded original SHA-256. A file Brain created is
deleted.

If you edited `config.toml` after configuring it, unconfigure refuses, leaves
the file as it is and keeps the record, so you can edit by hand.

No copy of your configuration is stored. The record and the pending state
hold hashes and fixed names only.

**File mode and attributes.** `config.toml` is replaced by a new file with
mode 0600 (configure and unconfigure alike). Because the original must
already be owner-only, its permission bits stay private, but ACLs, extended
attributes and any other inode metadata of the original are **not** preserved.

## Manual recovery (fail-closed states)

When configure or unconfigure refuses because `config.toml` drifted or was
deleted, the registered directory's identity changed, or the state under
`~/.brain/integrations/` is unsafe, Brain changes nothing and keeps its state.
There is no force option. To recover by hand:

1. Save any private changes you made to the registered home's `config.toml`
   somewhere safe first.
2. Inspect only what belongs to this label: `~/.brain/integrations/<label>.json`
   and `~/.brain/integrations/<label>.intent.json`, plus that one registered
   `config.toml`. Do not use wildcards, and do not touch Brain databases,
   auth files, trust state or other labels.
3. For a drifted configuration, restore the exact configured bytes recorded
   by Brain (for example from your own backup), then run `brain clients
   unconfigure --label <label> --yes`. A pre-configuration original will not
   match the recorded configured hash and will still be refused. For a valid
   pending operation, rerun the same configure or unconfigure command to
   finish the recovery that Brain reports.
4. Only if that is impossible, and you understand that the marked block
   between `# BRAIN_REGISTERED_MCP_BEGIN` and `# BRAIN_REGISTERED_MCP_END`
   must be removed by you from `config.toml`, remove that block, then delete
   exactly the two label state files named above.
5. Keep `~/.brain/integrations/bin/brain`; other labels may use it.

Brain does not automate these steps or delete arbitrary data.

## Crash safety

Before Brain first changes the integration binary or `config.toml`, it writes
`~/.brain/integrations/<label>.intent.json` (0600, fsynced together with its
directory).

Progress is derived only from the current hash of `config.toml`: original,
configured or neither. Running configure or unconfigure again under the
registry lock either finishes or rolls back an interrupted run.

**Recovery rules:**

| Situation | Result |
|---|---|
| `configure` while an unconfigure is pending | refused |
| `unconfigure` of an interrupted configure that never changed the file | drops the pending state |
| `unconfigure` of an interrupted configure that did change the file | first records it, then undoes it; the pending configure is never rewritten in place |
| the file changed while an operation was pending | refused; the pending state is kept |

**Limits:**

- **No multi-file atomicity.** There is no promise of atomicity across files.
- **Orphaned temporary files.** A crash between creating and renaming the
  owner-only temporary file next to `config.toml` (`.config.toml.brain-tmp-*`)
  can leave that file behind. It may contain the new configuration; remove it
  by hand.

## The integration binary

The MCP command points at `~/.brain/integrations/bin/brain`:

- **Location:** a 0700 file in a 0700 directory. It is separate from
  `~/.brain/bin/brain`, so removing the desktop shortcuts or running a default
  `brain uninstall`, including with 0.1.3 binaries, does not remove it.
- **Contents:** it is a copy of the exact executable bytes that were hashed.
  On Linux they are read through one handle on `/proc/self/exe`, which is the
  image this process is running, never through a path name that could
  already point at newer bytes. Other platforms refuse.
- **No automatic replacement:** it is never replaced. A different executable
  is refused. A missing copy is recreated only from an executable with the
  recorded hash.
- **Retained:** unconfigure intentionally leaves it in place. Remove the
  directory yourself once no label is configured.

## Forget and doctor

- **`forget`:** `brain clients forget` refuses while a record or a pending
  state exists for that label, including an unsafe or corrupt one.
- **`brain doctor`:** for each registration it shows `codex_mcp`, one of
  `none`, `configured`, `pending_recovery`, `unsafe_state` or
  `not_applicable`. For a Codex row, `integration` carries the same value,
  computed once; Claude Code and Hermes rows show `integration:
  not_implemented` and `codex_mcp: not_applicable`. A relative or
  control-character Brain home is reported as `unsafe_state` without reading
  it. See [client-profiles.md](client-profiles.md).
  - `configured` means a bounded, owner-only, schema-valid record matching
    the registration identity exists.
  - `pending_recovery` means a valid pending state exists.
  - Anything unreadable, corrupt or inconsistent is `unsafe_state`.

  Doctor reads only Brain's own files under `~/.brain/integrations/`, never
  the client's configuration, so `configured` does not mean Codex loads the
  entry.

## Residual risk

Checks and writes are separate steps. A process running as the same user, or
Codex itself writing `config.toml`, can change things between the final
re-check and the rename. Brain re-checks identity and content right before
replacing the file, but it is not a sandbox. The `HOME` check above is
lexical: a same-user process that later replaces a directory on an accepted
absolute path is outside this boundary.
