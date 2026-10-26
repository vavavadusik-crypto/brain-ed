#!/usr/bin/env python3
"""Offline, value-free secret audit of every reachable Git commit or working tree.

Default: consume git log -p --all (including merge-parent diffs), then inspect
every unique blob/path in every reachable tree and commit/tag metadata. No
textconv, external diff, network, checkout or history rewriting. Output contains
only locations, object IDs and rule names, never matching text. Exit codes:
0 = reviewed/clean, 1 = candidates requiring review, 2 = incomplete audit.
Exact blob/path/line/type exceptions live in release-secret-allowlist.json.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
GIT_ENV = dict(os.environ, GIT_NO_REPLACE_OBJECTS="1", GIT_NO_LAZY_FETCH="1",
               GIT_TERMINAL_PROMPT="0", GIT_OPTIONAL_LOCKS="0")
GIT = ["git", "-c", "core.fsmonitor=false", "-c", "core.pager=cat",
       "-c", "protocol.allow=never", "-c", "diff.external=",
       "-c", "log.showSignature=false", "-c", "core.hooksPath=/dev/null"]
KINDS = ["pem-private-key", "provider-key", "github-token", "slack-token",
         "aws-access-key", "service-key", "google-key", "jwt", "authorization",
         "url-password"]


def git(*args):
    return subprocess.check_output(GIT + list(args), cwd=ROOT, env=GIT_ENV,
                                   stderr=subprocess.DEVNULL)


def rules():
    # Import defines the project's masking functions; it does not open an
    # archive or invoke its CLI. Keep this tied to both production mask tables.
    spec = importlib.util.spec_from_file_location("release_mask", ROOT / "tools/session_archive.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    python = list(zip(KINDS, module.SECRET_PATTERNS))
    if len(module.SECRET_PATTERNS) != len(KINDS):
        raise ValueError("mask table changed; review scanner labels")
    rust = (ROOT / "brain-core/src/redact.rs").read_text()
    table = rust.split("const MASK_TABLE:", 1)[1].split("\n];", 1)[0]
    patterns = re.findall(r'r"([^"\n]*)"', table)
    if len(patterns) != len(KINDS):
        raise ValueError("Rust mask table changed; review scanner labels")
    result = python + [(kind, re.compile(pattern)) for kind, pattern in zip(KINDS, patterns)]
    # AWS temporary access IDs have the same shape as permanent IDs.
    result.append(("aws-temporary-key", re.compile(r"\bASIA[0-9A-Z]{16}\b")))
    result.append(("slack-app-token", re.compile(r"\bxapp-[0-9]+-[A-Za-z0-9]+-[A-Za-z0-9]+")))
    result.append(("slack-webhook", re.compile(r"https://hooks\.slack\.com/services/[A-Za-z0-9]+/[A-Za-z0-9]+/[A-Za-z0-9]+")))
    # Redis and similar URLs may omit the username before the password.
    result.append(("url-password", re.compile(
        r"(?i)\b[a-z][a-z0-9+.\-]{0,30}://[^\s:/@]{0,128}:(?!\[REDACTED\]@)[^\s/]+@")))
    return module, result


def scan(data, module, patterns):
    text = data.decode("utf-8", errors="replace")
    found = set()
    for kind, pattern in patterns:
        for match in pattern.finditer(text):
            # Rust delegates lookarounds to code; avoid already masked values.
            if kind == "url-password" and match.groupdict().get("value") == "[REDACTED]":
                continue
            found.add((text.count("\n", 0, match.start()) + 1, kind))
    for kind, pattern, helper in (
        ("credential-assignment", module.KEYED_SECRET, module._keyed_secret),
        ("credential-argument", module.SPOKEN_SECRET, module._spoken_secret),
    ):
        for match in pattern.finditer(text):
            if helper(match) != match.group():
                found.add((text.count("\n", 0, match.start()) + 1, kind))
    for match in module.FLAG_SECRET.finditer(text):
        if match.group("value")[1:-1] != "[REDACTED]":
            found.add((text.count("\n", 0, match.start()) + 1, "password-flag"))
    return sorted(found)


def identity(row):
    return tuple(row[k] for k in ("blob", "path", "line", "type"))


def audit(worktree=False, allowlist=None):
    if git("rev-parse", "--is-shallow-repository").strip() != b"false":
        raise ValueError("shallow history; full audit unavailable")
    module, patterns = rules()
    exceptions = {}
    if allowlist and allowlist.exists():
        for entry in json.loads(allowlist.read_text())["exceptions"]:
            if not entry.get("reason") or identity(entry) in exceptions:
                raise ValueError("invalid exception")
            exceptions[identity(entry)] = entry["reason"]
    findings, cache, seen = [], {}, set()
    commits = git("rev-list", "--all").decode().splitlines() if not worktree else []

    def inspect(blob, path, data, commit):
        key = (blob, path)
        if key in seen:
            return
        seen.add(key)
        if blob not in cache:
            cache[blob] = scan(data, module, patterns)
        matches = list(cache[blob])
        basename = Path(path).name
        if basename == ".env" or (basename.startswith(".env.") and
                                   basename not in (".env.example", ".env.sample", ".env.template")):
            matches.append((1, "env-file"))
        for line, kind in sorted(set(matches)):
            row = dict(commit=commit, blob=blob, path=path, line=line, type=kind)
            if identity(row) in exceptions:
                row["reviewed"] = exceptions[identity(row)]
            findings.append(row)

    def inspect_tree(tree, context):
        for record in git("ls-tree", "-r", "-z", tree).split(b"\0"):
            if not record:
                continue
            header, name = record.split(b"\t", 1)
            _, kind, object_id = header.split()
            if kind != b"blob":
                raise ValueError("submodule content is outside this repository")
            blob, path = object_id.decode(), os.fsdecode(name)
            if (blob, path) not in seen:
                inspect(blob, path, git("cat-file", "blob", blob), context)

    patch_digest = None
    if worktree:
        hash_name = git("rev-parse", "--show-object-format").decode().strip()
        files = git("ls-files", "--cached", "--others", "--exclude-standard", "-z").split(b"\0")
        for name in files:
            if not name:
                continue
            path = os.fsdecode(name)
            target = ROOT / path
            if target.is_symlink():
                data = os.fsencode(os.readlink(target))
            elif target.is_file():
                data = target.read_bytes()
            else:
                continue
            blob = hashlib.new(hash_name, b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
            inspect(blob, path, data, "WORKTREE")
    else:
        # Read the exact requested patch surface without ever emitting it.
        patch = git("log", "-p", "--all", "--full-history", "--root", "-m",
                    "--no-ext-diff", "--no-textconv", "--format=commit %H")
        patch_digest = hashlib.sha256(patch).hexdigest()
        # Trees are authoritative: patch-only scanning misses binary-marked
        # text, unchanged secrets and merge resolutions. Scan metadata too.
        for commit in commits:
            metadata = git("cat-file", "commit", commit)
            inspect(commit, "<commit-metadata>", metadata, commit)
            inspect_tree(commit, commit)
        tags_seen = set()
        for ref in git("for-each-ref", "--format=%(objecttype) %(objectname)", "refs/tags").decode().splitlines():
            kind, object_id = ref.split()
            while kind == "tag" and object_id not in tags_seen:
                tags_seen.add(object_id)
                data = git("cat-file", "tag", object_id)
                inspect(object_id, "<tag-metadata>", data, object_id)
                target = data.splitlines()[0].split()[1].decode()
                kind = git("cat-file", "-t", target).decode().strip()
                object_id = target
            # Git tags may name trees or blobs without any containing commit.
            # `rev-list --all` alone never visits their file contents.
            if kind == "tree":
                inspect_tree(object_id, object_id)
            elif kind == "blob":
                inspect(object_id, "<tag-blob>", git("cat-file", "blob", object_id), object_id)
    findings.sort(key=lambda row: (row["path"], row["commit"], row["line"], row["type"]))
    unreviewed = sum("reviewed" not in row for row in findings)
    return dict(scope="worktree" if worktree else "all-reachable-history",
                commits=len(commits), unique_objects=len(cache), blob_paths=len(seen),
                refs=git("for-each-ref", "--format=%(refname)").decode().splitlines(),
                patch_sha256=patch_digest, candidates=len(findings), unreviewed=unreviewed,
                findings=findings)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worktree", action="store_true", help="tracked and unignored working files")
    parser.add_argument("--allowlist", type=Path, default=ROOT / "tools/release-secret-allowlist.json")
    args = parser.parse_args()
    try:
        report = audit(args.worktree, args.allowlist)
    except Exception:
        # A syntax/import error or corrupt object must not print a traceback
        # containing a source line, a Git diagnostic or a credential value.
        print('{"error":"audit incomplete; check repository and mask tables"}', file=sys.stderr)
        return 2
    print(json.dumps(report, ensure_ascii=True, indent=2))
    return 1 if report["unreviewed"] else 0


if __name__ == "__main__":
    sys.exit(main())
