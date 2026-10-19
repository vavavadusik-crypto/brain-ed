# Registered client homes (foundation)

`brain clients` lets you tell Brain explicitly that a Codex, Claude Code or
Hermes home lives in a directory other than the default one. **Registration
only records and checks** that directory:

- Registering does not configure the client.
- It does not read the directory's contents, configuration, auth, `.env` or
  history.
- It does not discover or follow environment variables.
- It does not capture conversations from the registered directory.

The one integration built on top of a registration is separate and explicit:
for a registered **Codex** home, `brain clients configure --label L --yes`
adds a Brain MCP entry to that home's `config.toml`, and `brain clients
unconfigure` removes it. See [registered-codex-mcp.md](registered-codex-mcp.md).
Claude Code and Hermes registrations have no integration.

`brain init --agents`, `brain uninstall` and the archive collector never act
on registered homes. Default integration retains its existing scope; integration
and shortcut commands now also reject unsafe HOME before writes (see
[mcp-and-hooks.md](mcp-and-hooks.md)).

```console
brain clients register --client codex --home /abs/path/to/codex-home --label work-codex --dry-run
brain clients register --client codex --home /abs/path/to/codex-home --label work-codex
brain clients list                  # label, client, check; no paths
brain clients list --show-paths     # paths only when you ask
brain clients forget --label work-codex [--dry-run]
```

## Rules

### Opt-in only

Every registration is explicit, and there is no implicit replacement:

- `--client codex|claude|hermes`, `--home` and `--label` are all required.
- A duplicate label, or a directory that is already registered (by path or by
  device and inode), is refused.

### Limits

| Item | Limit |
|---|---|
| Label | `a-z`, `0-9` and `-`, 1 to 32 characters |
| Registrations | at most 64 |
| Registry file | at most 64 KiB |

### Home directory

The home must be:

- absolute and UTF-8, with no `.` or `..` segment anywhere in the text you
  pass (for example `/a/./b` is refused). Repeated and trailing slashes are
  accepted and stored in their canonical form;
- a real directory that exists;
- owned by you, with no group or other write access.

There must be no symlink on its path. The only exception is the components
that make up `HOME` itself, which may be an alias.

Brain stores the directory's device, inode and owner. If any of them changes,
the registration shows `identity_changed` and must be registered again; it is
not used.

### Registry file

- **Location:** `~/.brain/client-homes.json`, mode 0600, inside a private
  `~/.brain` (mode 0700, owned by you).
- **Writing:** the file is written atomically and is owner-only from its first
  byte.
- **Reading:** the read is bounded and does not follow symlinks.
- **Format:** strict schema version 1; unknown fields are an error.

### Concurrent changes

A real `register`, `forget`, `configure` or `unconfigure` holds an exclusive,
non-blocking lock on `~/.brain/client-homes.lock` for the whole read, check
and write of the registry (and, for `configure`/`unconfigure`, of the
integration state and `config.toml`); the lock is retained until the command
finishes. This prevents one change from overwriting another. While another
change holds the lock, the command refuses with "another registry change is
in progress" and changes nothing; run it again.

The lock file must be a private, empty, regular file owned by you. A symlink,
a directory, a non-empty file, a file with permissive modes or one owned by
someone else is refused. Brain never deletes the lock file. The lock is
released explicitly when the command finishes.

### Dry runs

`register --dry-run` and `forget --dry-run` write nothing. They do not create
`~/.brain`, the registry or the lock file. A dry run checks the registry as it
is at that moment and reserves nothing: a concurrent change can still win.
`list` and `brain doctor` are read-only and take no lock.

### Damaged or unsafe registry

A registry that is corrupt, oversized, a symlink, not owner-only, or inside an
unsafe Brain home is reported as `corrupt` or `unsafe`. `register`, `list` and
`forget` then refuse to act. Nothing falls back to environment variables or
defaults, and error messages never quote file contents or paths.

### Forget

`forget` refuses while the local install manifest records any file under that
home; run `brain uninstall` first.

The manifest is read as a bounded, private, no-follow file. Its fields and
types must match the installer's schema version 1. This registry check accepts
future registered-home paths and backup numbers beyond the default installer's
eight-path allowlist, but requires canonical path spelling:

- no unknown fields, and the exact field types;
- canonical absolute paths with no `.` or `..` and no duplicates;
- valid SHA-256 values;
- consistent `existed`, `before_sha256` and `backup` values;
- backup names of three digits plus `.bak`;
- no setuid, setgid or sticky modes.

An invalid manifest refuses the operation without changing the registry. The
manifest is the only data file read by this check; client files and backups are
not read. The client directory
itself is never touched.

## `brain doctor`

`brain doctor` adds a top-level `client_registry` section with:

- `state`: `absent`, `valid`, `unsafe` or `corrupt`;
- `reason`;
- `count`;
- one row per registration: `label`, `client`, `check`, `integration`,
  `codex_mcp` and `env_hint`.

`integration` and `codex_mcp` never disagree:

| Client | `integration` | `codex_mcp` |
|---|---|---|
| Codex | the same validated value as `codex_mcp` | `none`, `configured`, `pending_recovery` or `unsafe_state` |
| Claude Code, Hermes | `not_implemented` | `not_applicable` |

For Codex the value is computed once per row from Brain's own record under
`~/.brain/integrations/` only. `configured` means a valid Brain record exists;
the client's `config.toml` is not opened and no connection is tested, so it is
not proof that Codex loads the entry. Neither field claims conversation
capture.

`env_hint` is the variable name and its 0.1.3 state; it never contains a value
and never selects anything.

The existing per-client marker fields and their types are unchanged. With no
registry, the new section reports `state: "absent"`, `reason: null`, `count: 0`,
an empty registration list and the explicit-selection explanation. Diagnostic
limitations state that registration alone neither configures nor captures,
that Codex MCP is a separate explicit opt-in validated only through Brain's
own record, and that registered homes are otherwise inspected through
metadata only.

## Limits of these checks

- **TOCTOU.** Checks and later use are separate steps. A process running as
  the same user can change the directory in between. The stored identity
  narrows this window but does not close it, and Brain is not a sandbox.
- **Directories above the home.** Only symlinks are checked there, not their
  modes; a world-writable sticky `/tmp` above a home is accepted.
- **`HOME` alias exception.** A symlink is accepted in any component that is
  `HOME` itself or one of its ancestors (for example `/home -> /var/home`).
  Below `HOME`, and on paths outside it, symlinks are refused.
- **The lock covers registry changes only.** It does not protect the
  registered directories, and it protects nothing against processes that do
  not take it.
- **Client-side behaviour.** How each client uses its home — Codex
  `CODEX_HOME` and profiles, Claude Code `CLAUDE_CONFIG_DIR`, Hermes profiles
  and MCP — is outside this foundation, and is covered by future integration
  work against the clients' current documentation.

- **Real refusals may create an auxiliary lock.** A real `register` or `forget`
  may leave an empty private lock file when later validation refuses the
  operation. Existing registry data is preserved; dry runs still write nothing.
- **Noncanonical HOME spelling.** An installer manifest generated under a HOME
  spelling containing repeated separators or `.` may be valid for the default
  installer yet fail this stricter registry check. Complete its uninstall first,
  or use a canonical HOME spelling for a new installation; existing client files
  are not rewritten by registration.
- **Alternate aliases.** Forget checks lexical path prefixes; another spelling
  of the same HOME directory may not match. Registration itself has no client
  integration; the explicit Codex MCP opt-in refuses the default `~/.codex`
  by path or inode (see [registered-codex-mcp.md](registered-codex-mcp.md)).
- **Filesystem timing.** Explicit metadata inspection can wait on a remote
  mount. Network filesystem lock behaviour has not been tested.
- **`HOME` must be absolute.** Every `brain clients` subcommand (register,
  list, forget, configure, unconfigure, dry runs included) refuses with a fixed
  message, before any lock, registry read or write, when `HOME` or the Brain
  home is unset, empty, relative, not UTF-8 or contains a control character.
  `brain doctor` then reports `client_registry.state: "unsafe"` without
  reading the registry from that location. Absolute aliases, Unicode and
  spaces are unaffected. See the `HOME` section of
  [mcp-and-hooks.md](mcp-and-hooks.md) for the other guarded commands and for
  manual recovery after an earlier run with a bad `HOME`.
