# Linux download bundle (x86_64)

A bundle is a `.tar.gz` holding an already built `brain` executable, a small
installer and the licence files, so the two desktop icons can be set up
without Rust or a C compiler. **No bundle is published yet.** This page
describes the format and how to build and check one.

## What is in a bundle

```
brain-ed-<version>-linux-x86_64/
  bin/brain               the executable (no embedding model weights)
  install-linux.sh        installer for this user
  LICENSE  THIRD_PARTY_NOTICES.md  README.md  docs/INSTALL.md  docs/linux-download.md
  MANIFEST.json           version, source commit and tree, ELF linkage, per-file SHA-256
  SHA256SUMS              SHA-256 of every file above
```

## Install from a bundle

```console
tar -xzf brain-ed-<version>-linux-x86_64.tar.gz
cd brain-ed-<version>-linux-x86_64
./install-linux.sh --help
./install-linux.sh --dry-run      # verify, show planned paths, no installation changes
./install-linux.sh                # verify, then install for this user
./install-linux.sh --uninstall --dry-run
./install-linux.sh --uninstall    # launchers out; ~/.brain data kept
```

Before the binary runs, the installer checks that every entry of the unpacked
directory (the top directory included) is owned by you and not group- or
world-writable, that it holds exactly the expected files and directories (no symlinks, special files or
extras; any listing error stops it) and that `SHA256SUMS` is bounded, strict,
lists each file once and matches every file. `SHA256SUMS` is checked for NUL
bytes with a bounded read, then read once (bounded) as a snapshot whose byte
count must equal the file size measured just before. It then copies `bin/brain` into a fresh private `0700` temporary
directory, checks the copy against the snapshot's hash, and runs that copy's
own `brain ui --install-shortcut` (with `--dry-run`, nothing is installed;
only that private temporary copy is written, and it is removed on exit), which copies the binary
to `~/.brain/bin/brain` and writes two menu entries and, when an XDG desktop
directory exists, two desktop icons (Settings, Graph). It never uses sudo,
refuses root, downloads nothing, and does not install or enable agent
integration, the archive collector, any listener or an embedding model —
those stay separate explicit steps (see `docs/INSTALL.md`). `--uninstall`
re-verifies the bundle and runs the bundled binary's `brain ui
--remove-shortcut`; it removes only the launchers, and databases, settings,
configs, archives and models under `~/.brain` stay. Without the bundle you can
run `~/.brain/bin/brain ui --remove-shortcut` yourself: that installed copy is
trusted only as much as anything else your own user account can write; the
script never runs or checks it.

The private copy is staged under `$TMPDIR` (default `/tmp`). If that file
system is mounted `noexec`, the copy cannot run and installation stops; set
`TMPDIR` to an executable directory that only you can write (for example
`mkdir -m 700 ~/.cache/brain-tmp && TMPDIR=~/.cache/brain-tmp ./install-linux.sh`).
Do not work around it by running `bin/brain` directly or by loosening the
checksum or ownership checks.

These checks keep other accounts from swapping files between the check and
the run. They do not protect against a process running as your own user, and
the directory you unpacked into must itself be one only you can write; mode
checks alone do not remove every check-to-use race.

**Nothing here is provenance.** `SHA256SUMS`, `MANIFEST.json` and the
installer itself come from the same download, so the checks detect corruption
or a partial unpack, not a tampered release: whoever can replace the bundle
can replace its checksums and its installer too. The manifest records the
source commit the packager checked (a claim about the packaging step, not
proof of how the binary was compiled) and `source.repository`, the origin the
packager claimed, which is not validated against the source.

## Build a bundle

```console
cargo build --locked --release -p brain-cli
python3 tools/package-linux.py build --binary target/release/brain \
    --source . --commit "$(git rev-parse HEAD)" --out ../dist
python3 tools/package-linux.py verify ../dist/brain-ed-*-linux-x86_64.tar.gz
```

`--out` must be outside the source tree. `build` refuses, before writing
anything: a `--source` that is not the top of a plain clone (symlink,
gitfile, linked worktree, `.git/config.worktree`, `.git/modules`,
`.gitmodules`, any `extensions` setting or a `repositoryformatversion` other
than 0) or whose `.git/config` sets anything that can run a
program (filters, fsmonitor, hooks path, includes, pagers, credential or diff
helpers, …) — checked before git runs, and git itself runs with no global or
system config and those settings forced off; a tree with any tracked change or
untracked file; a `--commit` that is not the full HEAD id; a `--repository`
other than `https://host[:port]/path` (no userinfo, query or fragment); a
binary that is a symlink, FIFO or other non-regular file, grows while read, or
is not a well-formed x86-64 ELF executable; an existing archive or symlink at
the output name (never overwritten; staged and published with a no-clobber
link); an `--out` that is a symlink, not yours, or group/world-writable; and
any bundled or generated file (`MANIFEST.json`, `SHA256SUMS` included)
containing the packager's home (literal and resolved), the source, binary or
output path, or a recognisable secret token. Build with path remapping
(`--remap-path-prefix` in `RUSTFLAGS`, `-ffile-prefix-map` in `CFLAGS`) so
local paths do not end up in the binary. Text files are read from the
commit's Git objects. The archive is deterministic for identical inputs
(sorted members, owner 0/0, mtime = commit time).

`verify` is a packager and CI tool, not shipped in the bundle and not an
end-user trust check. It checks an archive before unpacking: bounded
compressed and decompressed size, no PAX or GNU long-name headers, canonical
unique paths with no type collisions, no setuid/setgid/sticky or
group/world-writable modes, the expected top directory and exact file set, a
bounded strict `SHA256SUMS` with unique rows, and a strict `MANIFEST.json`
(exact keys and types, no NaN/Infinity, bounded nesting) whose files agree
with the bundle and whose `target` agrees with the bundled ELF headers. At most
12 members are admitted, each checked against the known layout before its
content is read.

## Portability: unproven

- The binary is whatever `--binary` points at. A normal `cargo build` links
  glibc dynamically, so it needs at least the glibc of the build machine; the
  manifest records the highest `GLIBC_` symbol version found in the binary.
- A static musl build is configured in `.cargo/config.toml` but has not been
  built or tested for this bundle.
- No distribution has been tested with a bundle yet. Do not publish a binary
  built on a recent desktop distribution as "works on any Linux".
- Not supported: Windows, macOS, aarch64 and other architectures, 32-bit.

The optional workflow `.github/workflows/linux-bundle.yml` builds and checks a
bundle on `ubuntu-24.04` when started by hand and uploads it as a workflow
artifact only. It has read-only repository permissions and publishes nothing.
`actions/checkout` runs `git sparse-checkout disable`, which can leave
`.git/config.worktree` behind; the packager refuses that on purpose. The
workflow therefore makes an ordinary `git clone --no-hardlinks` of the checked-out
workspace into `$RUNNER_TEMP/src`, detaches it at exactly `$GITHUB_SHA`
(verified with `git rev-parse`), and builds, packages and tests from that
clone, with `target/` and `dist/` outside it. All packager checks stay
fail-closed and unchanged. A run on a real GitHub runner, the binary's
glibc/ABI floor and any provenance of the result are still unproven.
