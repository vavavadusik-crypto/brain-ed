#!/usr/bin/env python3
"""Package an already built `brain` executable as a Linux download bundle.

    python3 tools/package-linux.py build --binary PATH --source REPO \
        --commit FULL_SHA --out DIR
    python3 tools/package-linux.py verify ARCHIVE [--max-bytes N]

`build` never compiles anything. It takes the executable it is given and
records an association with one exact, clean Git commit: --source must be the
top of a Git work tree (not a symlink), with no tracked change and no
untracked file, HEAD must equal --commit, and every text file in the bundle is
read from that commit's Git objects (size checked before the blob is read).
The association is a recorded packaging claim. It does not prove the binary
was compiled from that commit; nothing here does.

Refused before anything is written: an unsafe --repository (only
https://host[:port]/path, no userinfo, query or fragment), a binary that is
not a regular file (opened once with O_NOFOLLOW|O_NONBLOCK, checked with
fstat, bounded, re-checked for growth) or not a well-formed x86-64 ELF
executable, an --out that is a symlink, is not a directory owned by the
caller, is group/world-writable or lies inside the source tree, an existing
archive (or symlink) at the target name, and any bundled or generated file
(including MANIFEST.json and SHA256SUMS) that contains the packager's home,
the source, binary or output path, or a recognisable secret token. The
archive is staged with O_EXCL|O_NOFOLLOW in --out and published with a
no-clobber hard link.

The archive is deterministic for the same inputs: sorted members, owner 0/0,
fixed modes, mtime = the commit time, gzip header without name or time.

`verify` checks an archive before it is unpacked: bounded compressed and
decompressed size, no PAX or GNU long-name extension headers, canonical
unique member paths with no type collisions, exactly the expected file and
directory set, a bounded strict SHA256SUMS with unique rows, a bounded strict
MANIFEST.json that agrees with it, and every file matching its checksum.
Checksums from the same download only detect corruption; they are not
provenance.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import tarfile

NAME = "brain-ed"
ARCH = "x86_64"
DEFAULT_REPOSITORY = "https://github.com/vavavadusik-crypto/brain-ed"
MAX_BINARY = 512 * 1024 * 1024
MAX_TEXT = 4 * 1024 * 1024
MAX_MANIFEST = 64 * 1024
MAX_SUMS_BYTES = 16 * 1024
MAX_SUMS_LINES = 64
MAX_ARCHIVE_BYTES = 768 * 1024 * 1024
MAX_MEMBERS = 12  # 9 files + top, bin, docs: anything beyond is refused before its body is read
# Text files taken from the commit; source path -> path inside the bundle.
FROM_COMMIT = {
    "LICENSE": "LICENSE",
    "THIRD_PARTY_NOTICES.md": "THIRD_PARTY_NOTICES.md",
    "README.md": "README.md",
    "docs/INSTALL.md": "docs/INSTALL.md",
    "docs/linux-download.md": "docs/linux-download.md",
    "packaging/install-linux.sh": "install-linux.sh",
}
EXECUTABLE = {"bin/brain", "install-linux.sh"}
PAYLOAD = set(FROM_COMMIT.values()) | {"bin/brain"}
BUNDLE_FILES = PAYLOAD | {"MANIFEST.json", "SHA256SUMS"}
BUNDLE_DIRS = {"bin", "docs"}
SECRET_PATTERNS = [
    re.compile(rb"gh[pousr]_[A-Za-z0-9]{36}"),
    re.compile(rb"github_pat_[A-Za-z0-9_]{60,}"),
    re.compile(rb"sk-ant-[A-Za-z0-9_-]{20,}"),
    re.compile(rb"sk-[A-Za-z0-9]{40,}"),
    re.compile(rb"AKIA[0-9A-Z]{16}"),
    re.compile(rb"xox[abpr]-[A-Za-z0-9-]{20,}"),
    # A key label followed by real key material, not the bare label a TLS
    # library carries as a constant.
    re.compile(rb"-----BEGIN [A-Z ]*PRIVATE KEY-----\s*[A-Za-z0-9+/]{40}"),
]
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
REPO_RE = re.compile(r"^https://[A-Za-z0-9](?:[A-Za-z0-9.-]{0,252}[A-Za-z0-9])?(?::[0-9]{1,5})?(?:/[A-Za-z0-9._~-]+){0,8}/?$")
# One path component and one canonical relative path inside a bundle.
PART = r"[A-Za-z0-9_][A-Za-z0-9._-]{0,99}"
PATH_RE = re.compile(rf"^{PART}(?:/{PART})*$")
SUMS_LINE = re.compile(r"^([0-9a-f]{64})  ([A-Za-z0-9._/-]{1,200})$")
# Git runs with a scrubbed environment, no global/system config, and the
# repository's command-running settings overridden on the command line.
GIT_ENV = {k: v for k, v in os.environ.items() if not k.startswith("GIT_") and k not in ("PAGER", "EDITOR", "VISUAL", "SSH_ASKPASS")}
GIT_ENV.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull,
                "GIT_TERMINAL_PROMPT": "0", "GIT_ATTR_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0", "GIT_NO_REPLACE_OBJECTS": "1", "LC_ALL": "C"})
GIT_SAFE = ["-c", "core.fsmonitor=false", "-c", f"core.hooksPath={os.devnull}", "-c", "protocol.allow=never",
            "-c", "core.untrackedCache=false", "-c", "diff.external=", "-c", "core.pager=cat",
            "-c", f"core.attributesFile={os.devnull}"]
# Repository-local settings that can make `git status` (or any later git
# command) run a program; a source that has any of them is refused before git
# is ever started on it.
RISKY_SECTION = re.compile(r"^\s*\[\s*(filter|include|includeif|diff|merge|credential|gpg|alias|url|sequence)\b", re.I | re.M)
RISKY_KEY = re.compile(r"^\s*(fsmonitor|hookspath|sshcommand|pager|editor|askpass|textconv|worktree|attributesfile|excludesfile|gitproxy|program)\s*=", re.I | re.M)
MANIFEST_KEYS = {"format", "name", "version", "target", "source", "association", "portability", "checksums", "files"}
TARGET_KEYS = {"os", "arch", "linkage", "interpreter", "max_glibc_symbol_version"}
SOURCE_KEYS = {"repository", "commit", "tree", "commit_time"}
MAX_JSON_DEPTH = 6
ASSOCIATION = ("recorded: the packager checked the source was clean at this commit; it does not prove the binary was built "
               "from it. source.repository is the origin the packager claimed; it is not validated against the source")
PORTABILITY = "UNPROVEN: no distribution or glibc floor has been tested for this binary"
CHECKSUMS_NOTE = "SHA256SUMS detects corruption only; it comes from the same download and is not provenance"


class Refused(Exception):
    pass


def canonical(path):
    if not PATH_RE.match(path) or any(p in (".", "..") for p in path.split("/")):
        raise Refused(f"non-canonical path {path!r}")
    return path


def git(repo, *args, limit=MAX_TEXT + 4096):
    result = subprocess.run(["git", *GIT_SAFE, "-C", repo, *args],
                            capture_output=True, check=False, env=GIT_ENV)
    if result.returncode != 0:
        raise Refused(f"git {' '.join(args)} failed: {result.stderr.decode(errors='replace').strip()[:400]}")
    if len(result.stdout) > limit:
        raise Refused(f"git {' '.join(args)} returned more than {limit} bytes")
    return result.stdout


def read_small(path, limit, what):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError as error:
        raise Refused(f"{what} cannot be opened safely ({error.strerror})")
    with os.fdopen(fd, "rb") as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise Refused(f"{what} must be a regular file of at most {limit} bytes")
        return handle.read(limit + 1)


# The only .git/config shape admitted: what plain `git init`, `git clone`,
# `git checkout` and actions/checkout write. Section -> allowed keys; True
# means the section must carry a "subsection". An empty key set admits only an
# empty section (actions/checkout can leave `[http "<url>"]` behind).
CONFIG_SECTIONS = {
    "core": (False, {"repositoryformatversion", "filemode", "bare", "logallrefupdates", "ignorecase", "precomposeunicode", "symlinks"}),
    "remote": (True, {"url", "fetch"}),
    "branch": (True, {"remote", "merge"}),
    "gc": (False, {"auto"}),
    "http": (True, set()),
}
CONFIG_HEADER = re.compile(r'\[([A-Za-z][A-Za-z0-9-]{0,63})(?: "([A-Za-z0-9._/+@:-]{1,200})")?\]')
CONFIG_KEY = re.compile(r"[ \t]*([A-Za-z][A-Za-z0-9-]{0,63})[ \t]*=[ \t]*(.*)")
CONFIG_VALUE = re.compile(r"[^ \t\"\\#;](?:[^\t\"\\#;]*[^ \t\"\\#;])?")


def config_bytes(raw):
    """Byte-level admission before anything else reads the text: printable
    ASCII, tab and LF only (no BOM, CR, NUL or other control or non-ASCII
    bytes), and a final newline."""
    if any(not (32 <= b < 127 or b in (9, 10)) for b in raw) or (raw and not raw.endswith(b"\n")):
        raise Refused(".git/config has bytes outside the plain-clone syntax (BOM, CR, control or non-ASCII); package from a plain clone")
    return raw.decode("ascii")


def config_lexical(config):
    """One whole section header or one `key = value` or comment per line; no
    adjacent or trailing-text headers, quotes, escapes, inline comments or
    continuations. Returns [(section, subsection, key)] for the allowlist."""
    entries, section = [], None
    for number, line in enumerate(config.split("\n")[:-1], 1):
        if not line.strip(" \t") or re.fullmatch(r"[ \t]*[#;].*", line):
            continue
        header = CONFIG_HEADER.fullmatch(line)
        if header:
            section = (header.group(1).lower(), header.group(2))
            entries.append((section[0], section[1], None))
            continue
        key = CONFIG_KEY.fullmatch(line)
        if not key or section is None or not CONFIG_VALUE.fullmatch(key.group(2)):
            raise Refused(f".git/config line {number} is outside the plain-clone syntax; package from a plain clone")
        entries.append((section[0], section[1], key.group(1).lower()))
    return entries


def config_allowlist(entries):
    for name, sub, key in entries:
        needs_sub, keys = CONFIG_SECTIONS.get(name, (None, None))
        if keys is None or needs_sub != (sub is not None) or (key is not None and key not in keys):
            shown = name + (f' "{sub}"' if sub else "") + (f".{key}" if key else "")
            raise Refused(f".git/config sets {shown!r}, which a plain clone does not; package from a plain clone")


def preflight(repo):
    """Checks done before any git command runs on --source."""
    info = os.lstat(repo)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise Refused(f"--source {repo} is not a directory (symlinks are refused)")
    dotgit = os.path.join(repo, ".git")
    try:
        git_info = os.lstat(dotgit)
    except FileNotFoundError:
        raise Refused("--source must be the top of a Git work tree with a .git directory")
    if stat.S_ISLNK(git_info.st_mode) or not stat.S_ISDIR(git_info.st_mode):
        raise Refused("--source/.git must be a real directory (gitfiles, linked worktrees and symlinks are refused)")
    if os.path.lexists(os.path.join(dotgit, "commondir")):
        raise Refused("linked worktrees are refused")
    # Other config sources and layouts git would read: refused outright, even
    # when they are symlinks or empty, rather than parsed.
    for extra in (os.path.join(dotgit, "config.worktree"), os.path.join(dotgit, "modules"), os.path.join(repo, ".gitmodules")):
        if os.path.lexists(extra):
            raise Refused(f"{os.path.relpath(extra, repo)} exists; worktree configs and submodules are refused (package from a plain clone)")
    config = config_bytes(read_small(os.path.join(dotgit, "config"), 64 * 1024, ".git/config"))
    format_lines = [l for l in config.split("\n") if "repositoryformatversion" in l.lower()]
    if any(not re.fullmatch(r"[ \t]*repositoryformatversion[ \t]*=[ \t]*0[ \t]*", l, re.I) for l in format_lines):
        raise Refused(".git/config repositoryformatversion must be exactly `repositoryformatversion = 0` (a plain clone)")
    entries = config_lexical(config)
    if re.search(r"^\s*\[\s*(extensions|submodule)\b", config, re.I | re.M) or "extensions." in config.lower():
        raise Refused(".git/config uses extensions or submodules; package from a plain clone")
    # Conservative, not a git-config parser: every line that mentions the key
    # at all (inline comments, quotes, bare keys, continuations, comment lines)
    # must be exactly the canonical `repositoryformatversion = 0`.
    for line in config.splitlines():
        if "repositoryformatversion" in line.lower() and not re.fullmatch(r"[ \t]*repositoryformatversion[ \t]*=[ \t]*0[ \t]*", line, re.I):
            raise Refused(".git/config repositoryformatversion must be exactly `repositoryformatversion = 0` (a plain clone)")
    risky = RISKY_SECTION.search(config) or RISKY_KEY.search(config)
    if risky:
        raise Refused(f".git/config sets {risky.group(1)!r}, which can run programs; package from a plain clone")
    config_allowlist(entries)


def source_association(repo, commit):
    if not SHA_RE.match(commit or ""):
        raise Refused("--commit must be a full 40-character lowercase hex commit id")
    preflight(repo)
    top = git(repo, "rev-parse", "--show-toplevel").decode().strip()
    if os.path.realpath(top) != os.path.realpath(repo):
        raise Refused(f"--source must be the top of the work tree ({top})")
    head = git(repo, "rev-parse", "--verify", "HEAD^{commit}").decode().strip()
    if head != commit:
        raise Refused(f"source HEAD {head} does not match --commit {commit}")
    dirty = git(repo, "status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none")
    if dirty.strip():
        raise Refused("source tree is not clean:\n" + dirty.decode(errors="replace")[:2000])
    tree = git(repo, "rev-parse", commit + "^{tree}").decode().strip()
    when = int(git(repo, "show", "-s", "--format=%ct", commit).decode().strip())
    return {"commit": commit, "tree": tree, "commit_time": when}


def blob(repo, commit, path):
    entry = git(repo, "ls-tree", "-z", commit, "--", path).split(b"\0")[0].decode()
    if not entry:
        raise Refused(f"{path} is not in commit {commit}")
    mode, kind = entry.split(" ")[:2]
    if kind != "blob" or mode not in ("100644", "100755"):
        raise Refused(f"{path} in {commit} is not a regular file (mode {mode})")
    size = int(git(repo, "cat-file", "-s", f"{commit}:{path}").decode().strip())
    if size > MAX_TEXT:
        raise Refused(f"{path} is larger than {MAX_TEXT} bytes")
    data = git(repo, "cat-file", "blob", f"{commit}:{path}", limit=size)
    if len(data) != size:
        raise Refused(f"{path} changed size while being read")
    return data


def workspace_version(repo, commit):
    text = blob(repo, commit, "Cargo.toml").decode(errors="replace")
    section = re.search(r"\[workspace\.package\](.*?)(\n\[|\Z)", text, re.S)
    match = re.search(r'^version\s*=\s*"([0-9][0-9A-Za-z.+-]{0,40})"', section.group(1) if section else "", re.M)
    if not match:
        raise Refused("no [workspace.package] version in Cargo.toml of the commit")
    return match.group(1)


def read_binary(path):
    """One descriptor, no symlink following, no blocking on a FIFO."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError as error:
        raise Refused(f"{path} cannot be opened safely ({error.strerror}); symlinks are refused")
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode):
            raise Refused(f"{path} is not a regular file")
        if before.st_size > MAX_BINARY:
            raise Refused(f"{path} is larger than {MAX_BINARY} bytes")
        chunks, total = [], 0
        while True:
            chunk = os.read(fd, min(1 << 20, before.st_size + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > before.st_size:
                raise Refused(f"{path} grew while being read")
        after = os.fstat(fd)
        if total != before.st_size or (after.st_size, after.st_mtime_ns) != (before.st_size, before.st_mtime_ns):
            raise Refused(f"{path} changed while being read")
    finally:
        os.close(fd)
    data = b"".join(chunks)
    elf_check(data, path)
    return data


def u(data, off, size):
    return int.from_bytes(data[off:off + size], "little")


def elf_check(data, path):
    if len(data) < 64 or data[:4] != b"\x7fELF":
        raise Refused(f"{path} is not an ELF file (or is truncated)")
    if data[4] != 2 or data[5] != 1 or data[6] != 1:
        raise Refused(f"{path} is not a 64-bit little-endian ELF (version 1)")
    if u(data, 16, 2) not in (2, 3) or u(data, 18, 2) != 62 or u(data, 20, 4) != 1:
        raise Refused(f"{path} is not an x86-64 executable (ET_EXEC/ET_DYN)")
    phoff, ehsize, phentsize, phnum = u(data, 32, 8), u(data, 52, 2), u(data, 54, 2), u(data, 56, 2)
    if ehsize != 64 or phentsize != 56 or not 1 <= phnum <= 128 or phoff < 64 or phoff + phnum * 56 > len(data):
        raise Refused(f"{path} has a malformed ELF program header table")
    loads = interps = 0
    for i in range(phnum):
        base = phoff + i * 56
        kind, off, filesz = u(data, base, 4), u(data, base + 8, 8), u(data, base + 32, 8)
        if kind in (1, 2, 3) and off + filesz > len(data):
            raise Refused(f"{path} has a program header pointing outside the file")
        loads += kind == 1
        if kind == 3:
            # Exactly one interpreter: an absolute path, one terminating NUL.
            interps += 1
            if interps > 1 or not re.fullmatch(rb"/[A-Za-z0-9._/+-]{1,4094}\0", data[off:off + filesz]):
                raise Refused(f"{path} has an empty, malformed or repeated PT_INTERP")
    if not loads:
        raise Refused(f"{path} has no loadable segment")


def elf_linkage(data):
    """From the program headers: PT_INTERP means dynamic; PT_DYNAMIC without
    an interpreter means static-pie; neither means static. The GLIBC_ value is
    the highest version string found by a scan, not a full symbol resolution."""
    phoff, phnum = u(data, 32, 8), u(data, 56, 2)
    interp, has_interp, has_dynamic = None, False, False
    for i in range(phnum):
        base = phoff + i * 56
        kind = u(data, base, 4)
        if kind == 3:
            off, size = u(data, base + 8, 8), u(data, base + 32, 8)
            interp, has_interp = data[off:off + size - 1].decode("ascii"), True  # validated by elf_check
        has_dynamic |= kind == 2
    versions = {tuple(int(x) for x in m.group(1).split(b".")) for m in re.finditer(rb"GLIBC_([0-9]{1,3}\.[0-9]{1,3}(?:\.[0-9]{1,3})?)\0", data)}
    top = ".".join(map(str, max(versions))) if versions else None
    linkage = "dynamic" if has_interp else ("static-pie" if has_dynamic else "static")
    return {"linkage": linkage, "interpreter": interp, "max_glibc_symbol_version": top}


def scan(path_in_bundle, data, forbidden):
    for needle in forbidden:
        if needle.encode() in data:
            raise Refused(f"{path_in_bundle} contains the private path {needle!r}; rebuild with path remapping")
    for pattern in SECRET_PATTERNS:
        if pattern.search(data):
            raise Refused(f"{path_in_bundle} contains something that looks like a secret")


def output_dir(out, repo):
    """--out must be an existing or creatable real directory owned by the
    caller, not group/world-writable, and outside the source tree."""
    out = os.path.abspath(out)
    parent = os.path.dirname(out)
    for path in (parent, out):
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            if path == parent:
                raise Refused(f"output parent {parent} does not exist")
            continue  # created only after every check has passed
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise Refused(f"{path} is not a directory (symlinks are refused)")
        if path == out and (info.st_uid != os.geteuid() or info.st_mode & 0o022):
            raise Refused(f"{out} must be owned by you and not group/world-writable")
    real = os.path.join(os.path.realpath(parent), os.path.basename(out))
    if real == repo or real.startswith(repo + os.sep):
        raise Refused("--out must be outside the source tree")
    return out


def write_no_clobber(directory, name, data):
    target = os.path.join(directory, name)
    if os.path.lexists(target):
        raise Refused(f"{target} already exists (files and symlinks are never overwritten)")
    try:
        os.mkdir(directory, 0o755)
    except FileExistsError:
        pass  # existing, or created by someone else since the preflight: checked below
    try:
        dfd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError as error:
        raise Refused(f"{directory} cannot be opened as a real directory ({error.strerror})")
    stage, created = f".{name}.{os.getpid()}.partial", False
    try:
        # The directory actually held, not the one seen by the preflight.
        held, named = os.fstat(dfd), os.lstat(directory)
        if (not stat.S_ISDIR(held.st_mode) or held.st_uid != os.geteuid() or held.st_mode & 0o022
                or (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino)):
            raise Refused(f"{directory} must be a directory owned by you, not group/world-writable, and not replaced while packaging")
        fd = os.open(stage, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o644, dir_fd=dfd)
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise Refused("staging file is not a regular file")
            created = True
            view = memoryview(data)
            while view:
                view = view[os.write(fd, view):]
            os.fsync(fd)
        finally:
            os.close(fd)
        try:
            os.link(stage, name, src_dir_fd=dfd, dst_dir_fd=dfd, follow_symlinks=False)
        except FileExistsError:
            raise Refused(f"{target} appeared while packaging; not overwritten")
        os.fsync(dfd)
    finally:
        if created:
            try:
                os.unlink(stage, dir_fd=dfd)
            except FileNotFoundError:
                pass
        os.close(dfd)
    return target


def build(args):
    if not REPO_RE.match(args.repository or "") or len(args.repository) > 200:
        raise Refused("--repository must be https://host[:port]/path with no userinfo, query or fragment")
    repo = os.path.realpath(args.source)
    assoc = source_association(args.source, args.commit)
    out = output_dir(args.out, repo)
    version = workspace_version(repo, args.commit)
    top = f"{NAME}-{version}-linux-{ARCH}"
    if os.path.lexists(os.path.join(out, top + ".tar.gz")):
        raise Refused(f"{os.path.join(out, top + '.tar.gz')} already exists (files and symlinks are never overwritten)")
    files = {name: blob(repo, args.commit, src) for src, name in FROM_COMMIT.items()}
    binary = read_binary(args.binary)
    files["bin/brain"] = binary
    literal_home = os.environ.get("HOME", "")
    forbidden = sorted({p for p in (literal_home, os.path.realpath(os.path.expanduser("~")), repo, os.path.abspath(args.source),
                                    os.path.realpath(os.path.dirname(os.path.abspath(args.binary))),
                                    os.path.dirname(os.path.abspath(args.binary)), out, os.path.realpath(os.path.dirname(out)))
                        if p.startswith("/") and len(p) > 1})
    for name, data in files.items():
        scan(name, data, forbidden)
    manifest = {
        "format": 1,
        "name": NAME,
        "version": version,
        "target": {"os": "linux", "arch": ARCH, **elf_linkage(binary)},
        "source": {"repository": args.repository, **assoc},
        "association": ASSOCIATION,
        "portability": PORTABILITY,
        "checksums": CHECKSUMS_NOTE,
        "files": [{"path": n, "sha256": hashlib.sha256(d).hexdigest(), "size": len(d)} for n, d in sorted(files.items())],
    }
    files["MANIFEST.json"] = (json.dumps(manifest, indent=1, sort_keys=True) + "\n").encode()
    files["SHA256SUMS"] = "".join(f"{hashlib.sha256(d).hexdigest()}  {n}\n" for n, d in sorted(files.items())).encode()
    for name in ("MANIFEST.json", "SHA256SUMS"):  # generated metadata is scanned too
        scan(name, files[name], forbidden)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for d in sorted({top} | {f"{top}/{x}" for x in BUNDLE_DIRS}):
            info = tarfile.TarInfo(d)
            info.type, info.mode, info.mtime = tarfile.DIRTYPE, 0o755, assoc["commit_time"]
            tar.addfile(info)
        for name in sorted(files):
            info = tarfile.TarInfo(f"{top}/{name}")
            info.size, info.mtime = len(files[name]), assoc["commit_time"]
            info.mode = 0o755 if name in EXECUTABLE else 0o644
            tar.addfile(info, io.BytesIO(files[name]))
    packed = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=packed, mtime=0) as gz:
        gz.write(raw.getvalue())
    archive = write_no_clobber(out, top + ".tar.gz", packed.getvalue())
    print(json.dumps({"archive": archive, "sha256": hashlib.sha256(packed.getvalue()).hexdigest(), "version": version,
                      **assoc, "target": manifest["target"]}, indent=1))


def bounded_gunzip(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise Refused(f"{path} is not a regular file of at most {limit} bytes")
        with gzip.GzipFile(fileobj=handle, mode="rb") as gz:
            data = gz.read(limit + 1)  # bounded: never more than limit + 1 bytes decompressed
    if len(data) > limit:
        raise Refused(f"archive decompresses to more than {limit} bytes")
    return data


def strict_json(body):
    """Duplicate keys, NaN/Infinity (and overflowing floats), oversized
    integers and nesting deeper than MAX_JSON_DEPTH are refused."""
    def no_duplicates(pairs):
        keys = [k for k, _ in pairs]
        if len(keys) != len(set(keys)):
            raise Refused("MANIFEST.json has duplicate keys")
        return dict(pairs)

    def no_constant(name):
        raise Refused(f"MANIFEST.json uses the non-finite constant {name}")

    def finite(text):
        value = float(text)
        if value != value or value in (float("inf"), float("-inf")):
            raise Refused(f"MANIFEST.json has a non-finite number {text[:20]}")
        return value
    try:
        value = json.loads(body.decode("utf-8"), object_pairs_hook=no_duplicates, parse_constant=no_constant, parse_float=finite)
    except RecursionError:
        raise Refused("MANIFEST.json is too deeply nested")
    except (UnicodeDecodeError, ValueError) as error:  # JSONDecodeError and the int-digits limit are ValueErrors
        raise Refused(f"MANIFEST.json is not valid JSON: {str(error)[:120]}")
    stack = [(value, 1)]
    while stack:
        item, depth = stack.pop()
        if depth > MAX_JSON_DEPTH:
            raise Refused("MANIFEST.json is too deeply nested")
        if type(item) is dict:
            stack.extend((v, depth + 1) for v in item.values())
        elif type(item) is list:
            stack.extend((v, depth + 1) for v in item)
    return value


def exact_object(value, keys, what):
    if type(value) is not dict or set(value) != keys:
        raise Refused(f"MANIFEST.json {what} must be an object with exactly the keys {sorted(keys)}")


def bounded_text(value, what, pattern, limit):
    if type(value) is not str or not 0 < len(value) <= limit or not re.fullmatch(pattern, value):
        raise Refused(f"MANIFEST.json {what} must be a string of the expected form (at most {limit} characters)")
    return value


def bounded_int(value, what, low, high):
    if type(value) is not int or not low <= value <= high:  # bool is not accepted as int
        raise Refused(f"MANIFEST.json {what} must be an integer in [{low}, {high}]")
    return value


def check_manifest(manifest, files, top):
    """Exact types and values; every field is checked before it is used."""
    exact_object(manifest, MANIFEST_KEYS, "top level")
    bounded_int(manifest["format"], "format", 1, 1)
    if manifest["name"] != NAME or type(manifest["name"]) is not str:
        raise Refused(f"MANIFEST.json name must be {NAME!r}")
    version = bounded_text(manifest["version"], "version", r"[0-9][0-9A-Za-z.+-]{0,40}", 41)
    for key, expected in (("association", ASSOCIATION), ("portability", PORTABILITY), ("checksums", CHECKSUMS_NOTE)):
        if type(manifest[key]) is not str or manifest[key] != expected:
            raise Refused(f"MANIFEST.json {key} must be the standard statement")
    source = manifest["source"]
    exact_object(source, SOURCE_KEYS, "source")
    repo = bounded_text(source["repository"], "source.repository", r"https://[^\s]{1,192}", 200)
    if not REPO_RE.match(repo):
        raise Refused("MANIFEST.json source.repository is not https://host[:port]/path")
    bounded_text(source["commit"], "source.commit", r"[0-9a-f]{40}", 40)
    bounded_text(source["tree"], "source.tree", r"[0-9a-f]{40}", 40)
    bounded_int(source["commit_time"], "source.commit_time", 0, 2 ** 40)
    target = manifest["target"]
    exact_object(target, TARGET_KEYS, "target")
    elf_check(files["bin/brain"], "bin/brain")
    checked = {"os": "linux", "arch": ARCH, **elf_linkage(files["bin/brain"])}
    if any(type(target[k]) is not type(checked[k]) or target[k] != checked[k] for k in TARGET_KEYS):
        raise Refused("MANIFEST.json target does not match the bundled ELF")
    entries = manifest["files"]
    if type(entries) is not list or len(entries) != len(PAYLOAD):
        raise Refused(f"MANIFEST.json files must be a list of {len(PAYLOAD)} entries")
    described = {}
    for entry in entries:
        exact_object(entry, {"path", "sha256", "size"}, "files entry")
        path = canonical(bounded_text(entry["path"], "files path", r"[A-Za-z0-9_][A-Za-z0-9._/-]*", 200))
        digest = bounded_text(entry["sha256"], "files sha256", r"[0-9a-f]{64}", 64)
        size = bounded_int(entry["size"], "files size", 0, MAX_BINARY)
        if path in described:
            raise Refused(f"MANIFEST.json lists {path} twice")
        described[path] = (digest, size)
    if described != {n: (hashlib.sha256(files[n]).hexdigest(), len(files[n])) for n in PAYLOAD}:
        raise Refused("MANIFEST.json files do not match the bundle")
    if top != f"{NAME}-{version}-linux-{ARCH}":
        raise Refused(f"unexpected top directory {top!r} for version {version!r}")
    return manifest


def verify(args):
    raw = bounded_gunzip(args.archive, args.max_bytes)
    files, dirs, kinds, top, expected = {}, set(), {}, None, 0
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as tar:
        for member in tar:
            # Headers must follow each other with nothing in between: tarfile
            # silently consumes global (g) and other extension headers, which
            # shows up as a gap before the member's own header.
            if member.offset != expected:
                raise Refused(f"member {member.name!r} is preceded by an extension or global header (offset {member.offset}, expected {expected})")
            expected = member.offset_data + tarfile.BLOCKSIZE * -(-member.size // tarfile.BLOCKSIZE)
            if member.offset_data - member.offset != tarfile.BLOCKSIZE or member.pax_headers:
                raise Refused(f"member {member.name!r} uses a PAX or GNU long-name extension header")
            name = canonical(member.name)
            parts = name.split("/")
            top = top or parts[0]
            if parts[0] != top:
                raise Refused(f"member {name!r} outside the single top directory {top!r}")
            rel = "/".join(parts[1:])
            if member.mode & 0o7000 or member.mode & 0o022:
                raise Refused(f"member {name!r} has an unsafe mode {oct(member.mode)} (setuid/setgid/sticky or group/world-writable)")
            # Raw type only: isreg() would also admit contiguous (7) and GNU sparse (S) members.
            kind = "dir" if member.type == tarfile.DIRTYPE else "file" if member.type in (tarfile.REGTYPE, tarfile.AREGTYPE) else None
            if kind is None:
                raise Refused(f"member {name!r} is not a regular file or directory")
            if name in kinds:
                raise Refused(f"duplicate member {name!r}")
            for i in range(1, len(parts)):
                if kinds.get("/".join(parts[:i])) == "file":
                    raise Refused(f"member {name!r} collides with a file of the same prefix")
            kinds[name] = kind
            known = rel in ({""} | BUNDLE_DIRS) if kind == "dir" else rel in BUNDLE_FILES
            if not known or len(kinds) > MAX_MEMBERS:
                raise Refused(f"unexpected bundle layout: member {name!r} is not part of a bundle (at most {MAX_MEMBERS} members)")
            if kind == "dir":
                dirs.add(rel)
                continue
            cap = {"bin/brain": MAX_BINARY, "MANIFEST.json": MAX_MANIFEST, "SHA256SUMS": MAX_SUMS_BYTES}.get(rel, MAX_TEXT)
            if member.size > cap:
                raise Refused(f"member {name!r} declares {member.size} bytes, more than the {cap} allowed for {rel}")
            body = tar.extractfile(member).read(member.size + 1)
            if len(body) != member.size:
                raise Refused(f"member {name!r} size mismatch")
            files[rel] = body
    if set(files) != BUNDLE_FILES or dirs != {""} | BUNDLE_DIRS:
        raise Refused(f"unexpected bundle layout: missing {sorted(BUNDLE_FILES - set(files))}, extra {sorted((set(files) - BUNDLE_FILES) | (dirs - {''} - BUNDLE_DIRS))}")
    sums = files["SHA256SUMS"]
    lines = sums.decode("ascii", errors="replace").split("\n")
    if len(sums) > MAX_SUMS_BYTES or lines[-1] != "" or not 1 <= len(lines) - 1 <= MAX_SUMS_LINES:
        raise Refused(f"SHA256SUMS must be at most {MAX_SUMS_BYTES} bytes and {MAX_SUMS_LINES} newline-terminated lines")
    listed = {}
    for line in lines[:-1]:
        match = SUMS_LINE.match(line)
        if not match:
            raise Refused(f"malformed SHA256SUMS line {line!r}")
        path = canonical(match.group(2))
        if path in listed:
            raise Refused(f"duplicate SHA256SUMS row for {path}")
        listed[path] = match.group(1)
    if set(listed) != BUNDLE_FILES - {"SHA256SUMS"}:
        raise Refused("SHA256SUMS does not list exactly the bundle files")
    for name, digest in listed.items():
        if hashlib.sha256(files[name]).hexdigest() != digest:
            raise Refused(f"checksum mismatch for {name}")
    if len(files["MANIFEST.json"]) > MAX_MANIFEST:
        raise Refused(f"MANIFEST.json is larger than {MAX_MANIFEST} bytes")
    manifest = check_manifest(strict_json(files["MANIFEST.json"]), files, top)
    print(json.dumps({"archive": args.archive, "top": top, "files": len(files), "bytes": len(raw), "status": "ok",
                      "commit": manifest["source"]["commit"], "note": "checksums detect corruption only, not provenance"}))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    b = sub.add_parser("build", help="bundle an already built executable")
    b.add_argument("--binary", required=True)
    b.add_argument("--source", required=True, help="top of a clean Git work tree at --commit")
    b.add_argument("--commit", required=True, help="full 40-hex commit id HEAD must equal")
    b.add_argument("--out", required=True, help="output directory outside the source tree")
    b.add_argument("--repository", default=DEFAULT_REPOSITORY, help="https URL recorded in the manifest")
    v = sub.add_parser("verify", help="check an archive before unpacking")
    v.add_argument("archive")
    v.add_argument("--max-bytes", type=int, default=MAX_ARCHIVE_BYTES)
    args = parser.parse_args(argv)
    try:
        (build if args.command == "build" else verify)(args)
    except (Refused, tarfile.TarError, OSError, EOFError, gzip.BadGzipFile, ValueError) as error:
        print(f"package-linux: refused: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
