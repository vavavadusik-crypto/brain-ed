[RU version](kimi-code.ru.md)

# Connecting Kimi Code

What `brain init --agents` does for Kimi Code, how to verify the connection, and how to roll it back. All paths are in the user's home; the `brain` command is the stable copy the installer puts in `~/.brain/bin`.

## What is written

- `~/.kimi-code/mcp.json` — ключ "mcpServers"."brain" — команда `<brain> serve`

## Hooks

Hooks: нет (хуки Kimi Code не умеют добавлять контекст).

## How to verify

```console
$ brain doctor
{
  "settings_agents": [ { "id": "kimi", "connected": true }, … ],
  "clients": [ { "label": "Kimi Code", "mcp_config_entry_present": true, … } ]
}
```

`brain doctor` is read-only. `settings_agents` shows `connected: true` when the
install manifest holds entries for this client's files;
`mcp_config_entry_present` says the client's config carries the Brain entry.
The settings window offers the same: `brain ui` → "Агенты" section — plan
diffs (`--dry-run` equivalent) and confirmed connect/disconnect per agent.

## How to disconnect or roll back

```console
$ brain uninstall --dry-run      # show the restore plan
$ brain uninstall --yes          # restore the original files from backups
Uninstalled Brain integration from 12 file(s).
```

`brain uninstall` restores each file to its pre-install bytes (checksums in the
manifest) and removes files Brain created. A single agent can be disconnected
from the settings window (`brain ui` → agents) — the same uninstall code,
narrowed to that agent; the other agents' manifest entries stay.

## Common problems

- **The client home is relocated (e.g. `CLAUDE_HOME` points elsewhere)** — Such a client is skipped by the planner with a relocation note — nothing is written; move the variable back or align the location.
- **`an installation manifest already exists and files drifted`** — Files changed after the install. Inspect `--dry-run`, revert your edits, or run `brain uninstall` and connect again.
- **Hooks do not fire** — Restart the client: configs are read at startup. Check `brain doctor` (hook_config_marker_present) and the `brain journal …` log.
- **`file changed after installation; leaving it untouched` during uninstall** — The file was edited after installation; uninstall deliberately skips it. Restore the file or edit it manually.
- **The client is invisible to `brain init --agents`** — Detection uses the client's directory or its executable on PATH; create the directory (`{dir}`) or install the client CLI.

Только MCP.

The common flow: `brain init --agents --dry-run` → review the diff → `brain init --agents --yes` (or confirm in the settings window).
