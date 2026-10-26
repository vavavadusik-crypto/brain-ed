#!/usr/bin/env python3
"""Check that every relative Markdown link under docs/ points at an existing file.

Deterministic, offline: only the local filesystem is consulted. Links inside
fenced code blocks and inline code are ignored, as are absolute URLs,
mailto: targets and pure-anchor links. A `#fragment` is stripped before the
target file is resolved; the fragment itself is not validated.

Usage: python3 tools/docs_link_check.py [repo-root]   (default: repo root)
Exit code 0 when every link resolves, 1 otherwise.
"""

import os
import re
import sys

LINK_RE = re.compile(r"\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
FENCE_RE = re.compile(r"^(```|~~~)")


def strip_fences(text: str) -> str:
    out = []
    fence = None
    for line in text.splitlines():
        m = FENCE_RE.match(line.strip())
        if fence:
            if m and line.strip().startswith(fence):
                fence = None
            continue
        if m:
            fence = m.group(1)[0] * 3
            continue
        out.append(line)
    return "\n".join(out)


def iter_markdown(root: str):
    for dirpath, dirnames, files in os.walk(root):
        dirnames.sort()
        for name in sorted(files):
            if name.endswith(".md"):
                yield os.path.join(dirpath, name)


def main() -> int:
    repo = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    docs = os.path.join(repo, "docs")
    if not os.path.isdir(docs):
        print(f"docs/ not found under {repo}", file=sys.stderr)
        return 2

    checked = broken = 0
    for path in iter_markdown(docs):
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
        text = strip_fences(text)
        rel_dir = os.path.dirname(path)
        for m in LINK_RE.finditer(text):
            target = m.group(1).strip()
            if target.startswith(("http://", "https://", "mailto:", "#")):
                continue
            if "://" in target:
                continue
            fragment = ""
            if "#" in target:
                target, fragment = target.split("#", 1)
            if not target:
                continue
            checked += 1
            resolved = os.path.normpath(os.path.join(rel_dir, target))
            if not os.path.exists(resolved):
                broken += 1
                print(f"BROKEN {os.path.relpath(path, repo)} -> {target}")
            elif fragment and resolved == os.path.normpath(path):
                pass  # same-file anchor: existence of the file is checked, anchor text is not

    print(f"checked {checked} relative links in docs/, broken: {broken}")
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main())
