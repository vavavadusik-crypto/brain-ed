# Local backups and restore (Linux)

`brain backup create` snapshots each existing SQLite database with `VACUUM
INTO`, including committed WAL data. It copies settings through an owner-only
file. Newly created backup directories are 0700; files are 0600 from creation,
including with umask 000. An interrupted create leaves an unlisted `.pending-*`
directory, never a published partial snapshot.

`brain backup prune --keep N` orders snapshots by persisted creation metadata,
with filesystem birth time (or mtime) for legacy directories. The stored time
advances even if the wall clock moves backwards and survives restore. Ties at
the retention boundary are kept conservatively. Symlink directories are never
snapshots. At least one directory and the newest usable image are protected,
even if newer named directories are empty or corrupt.

`brain backup restore SNAPSHOT` validates its inputs and reports a dry run.
Add `--yes` to apply. Stop Brain services and other SQLite clients first:
restore requires Linux file leases and refuses any open database holder,
including idle WAL/read-only connections. Unsupported filesystems fail closed.
Paths with symlink components, symlink database sidecars, non-regular restored
entries and `..` components are refused. Unrelated symlinks in the home are
preserved as links without following or chmodding their targets.

Restore prepares a private sibling directory on the same filesystem. It
recreates the directory tree, hard-links unchanged regular files, and writes
new private SQLite images and settings. The staged restored databases have no
old WAL/SHM/journal files. Before committing, it obtains kernel write leases
on the live databases, settings and existing database sidecars. Leases refuse
existing holders and block new opens until the exchange finishes. A competing
open detected before the commit causes a refusal.

One `renameat2(RENAME_EXCHANGE)` swaps the entire Brain home with the prepared
directory. There is no interval between replacing the two databases: process
death leaves either the entire original home or the entire restored home
immediately usable, without requiring a subsequent recovery command. Preparation
or exchange errors keep the original live files. The source snapshot is never
consumed. Backup operations use an exclusive directory lock while preparing.

The result reports a normal `safety_snapshot` and `previous_home`: a private
sibling containing the displaced tree. Keep it until the restore is verified.
Unchanged files in the two trees share inodes, so `previous_home` is **not** an
independent immutable backup of unrelated data. No automatic deletion of the
previous tree or interrupted staging directories is performed. Their directory
entries add work and can retain disk space after snapshot pruning; account for
them when managing disk usage. Restore refuses special files such as active
service sockets in the home. Filesystems must support hard links, file leases
and atomic directory exchange; restore does not silently fall back to separate
file renames or copy the whole archive's bytes.

New images and staged directories are synced before exchange. `parent_synced`
reports the parent-directory sync after commit; false means the directory
exchange is visible but its power-loss durability was not confirmed. Regression
tests inject process death and syscall errors. They do not simulate power cuts
or prove safety against arbitrary concurrent path replacement by the same user.

Process regressions (synthetic temporary HOME only):

```sh
BRAIN_PROBE_BIN=/path/to/debug/brain python3 -B tests/test_backup_process.py
```

They require Linux, a C compiler and `strace`. The preload shim is compiled in
a temporary directory and is never installed or loaded in normal operation.
