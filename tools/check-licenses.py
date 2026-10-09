#!/usr/bin/env python3
"""Dependency licence policy check for the Brain ED workspace.

Reads `cargo metadata --locked` (no extra tool needed) and parses each
package's SPDX licence expression properly: `OR` is a choice offered by the
author (one acceptable branch is enough), `AND` binds every part, `WITH`
attaches an exception, parentheses group, and the legacy `A/B` spelling means
`A OR B`. The policy is an allowlist: an identifier nobody reviewed fails the
check instead of passing silently.

This is a policy gate, not legal advice. It does not claim that any single
licence makes the whole binary copyleft; MPL-2.0, for example, is file-level
copyleft and is accepted only for unmodified upstream crates. Redistributors
still have to ship the notices listed in THIRD_PARTY_NOTICES.md.

Usage:
    python3 tools/check-licenses.py              # check the workspace
    python3 tools/check-licenses.py --self-test  # parser tests, no cargo
"""

import hashlib
import json
import pathlib
import re
import subprocess
import sys

PERMISSIVE = {
    "0BSD",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "BSL-1.0",
    "CC0-1.0",
    "CDLA-Permissive-2.0",
    "ISC",
    "MIT",
    "MIT-0",
    "Unicode-3.0",
    "Unlicense",
    "Zlib",
}
# File-level copyleft, acceptable for unmodified third-party source files.
WEAK_COPYLEFT_ACCEPTED = {"MPL-2.0"}
ACCEPTED_EXCEPTIONS = {"LLVM-exception"}

# Crates whose metadata names a `license_file` instead of an SPDX `license`.
# Each file was read by hand; the hash pins exactly the text that was read, so
# a new version or a changed file fails until someone reads it again.
VERIFIED_LICENCE_FILES = {
    ("model2vec-rs", "0.2.1"): (
        "MIT",
        "6869ad291a9df9866448579b7157bf337373a21ced5b4b315e68b2924aff2ab9",
    ),
}

TOKEN = re.compile(r"\s*(\(|\)|[A-Za-z0-9.+\-]+)")


class LicenceError(ValueError):
    pass


def tokenize(expression):
    expression = expression.replace("/", " OR ")
    tokens, position = [], 0
    while position < len(expression):
        if expression[position:].strip() == "":
            break
        match = TOKEN.match(expression, position)
        if not match:
            raise LicenceError(f"cannot parse licence expression {expression!r}")
        tokens.append(match.group(1))
        position = match.end()
    return tokens


def parse(expression):
    """Returns a tree: ("or", [..]) | ("and", [..]) | ("id", name, exception)."""
    tokens = tokenize(expression)
    index = 0

    def peek():
        return tokens[index] if index < len(tokens) else None

    def take():
        nonlocal index
        token = peek()
        if token is None:
            raise LicenceError(f"unexpected end of {expression!r}")
        index += 1
        return token

    def parse_or():
        parts = [parse_and()]
        while peek() is not None and peek().upper() == "OR":
            take()
            parts.append(parse_and())
        return parts[0] if len(parts) == 1 else ("or", parts)

    def parse_and():
        parts = [parse_atom()]
        while peek() is not None and peek().upper() == "AND":
            take()
            parts.append(parse_atom())
        return parts[0] if len(parts) == 1 else ("and", parts)

    def parse_atom():
        token = take()
        if token == "(":
            node = parse_or()
            if take() != ")":
                raise LicenceError(f"unbalanced parentheses in {expression!r}")
            return node
        if token == ")" or token.upper() in {"AND", "OR", "WITH"}:
            raise LicenceError(f"unexpected {token!r} in {expression!r}")
        exception = None
        if peek() is not None and peek().upper() == "WITH":
            take()
            exception = take()
        return ("id", token, exception)

    tree = parse_or()
    if peek() is not None:
        raise LicenceError(f"trailing {peek()!r} in {expression!r}")
    return tree


def acceptable(node):
    kind = node[0]
    if kind == "or":
        return any(acceptable(child) for child in node[1])
    if kind == "and":
        return all(acceptable(child) for child in node[1])
    _, name, exception = node
    name = name.removesuffix("+")
    if exception is not None and exception not in ACCEPTED_EXCEPTIONS:
        return False
    return name in PERMISSIVE or name in WEAK_COPYLEFT_ACCEPTED


def licence_is_usable(expression):
    return acceptable(parse(expression))


def self_test():
    cases = {
        "MIT": True,
        "MIT OR Apache-2.0": True,
        "MIT/Apache-2.0": True,
        "Apache-2.0 / MIT": True,
        "(MIT OR Apache-2.0) AND Unicode-3.0": True,
        "Apache-2.0 AND ISC": True,
        "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT": True,
        "MIT OR Apache-2.0 OR LGPL-2.1-or-later": True,
        "MPL-2.0": True,
        "GPL-3.0-only": False,
        "LGPL-2.1-or-later": False,
        "MIT AND GPL-2.0-only": False,
        "(MIT OR Apache-2.0) AND AGPL-3.0-only": False,
        "(GPL-2.0-only OR GPL-3.0-only) AND MIT": False,
        "SSPL-1.0 OR BUSL-1.1": False,
        "GPL-2.0-only WITH Classpath-exception-2.0": False,
        "Some-Unreviewed-Licence": False,
    }
    failures = [
        (expression, expected)
        for expression, expected in cases.items()
        if licence_is_usable(expression) != expected
    ]
    for bad in ("MIT OR", "(MIT", "MIT)", "AND MIT", "MIT $ Apache-2.0"):
        try:
            parse(bad)
            failures.append((bad, "should not parse"))
        except LicenceError:
            pass
    for expression, expected in failures:
        print(f"SELF-TEST FAIL  {expression!r}: expected {expected}", file=sys.stderr)
    if failures:
        return 1
    print(f"{len(cases) + 5} licence-expression cases passed.")
    return 0


def file_sha256(package):
    directory = pathlib.Path(package["manifest_path"]).parent
    path = directory / package["license_file"]
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    if "--self-test" in sys.argv[1:]:
        return self_test()

    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )

    problems = []
    for package in metadata["packages"]:
        name, version = package["name"], package["version"]
        licence = package.get("license")
        if licence:
            try:
                if not licence_is_usable(licence):
                    problems.append(f"UNUSABLE    {name} {version}: {licence}")
            except LicenceError as error:
                problems.append(f"UNPARSEABLE {name} {version}: {error}")
            continue
        if package.get("license_file"):
            verified = VERIFIED_LICENCE_FILES.get((name, version))
            if verified is None:
                problems.append(f"UNREVIEWED  {name} {version}: license_file only")
                continue
            try:
                digest = file_sha256(package)
            except OSError as error:
                problems.append(f"UNREADABLE  {name} {version}: {error}")
                continue
            if digest != verified[1]:
                problems.append(f"CHANGED     {name} {version}: licence file hash {digest}")
            continue
        problems.append(f"UNDECLARED  {name} {version}")

    for line in problems:
        print(line, file=sys.stderr)
    if problems:
        print(f"\n{len(problems)} dependency licence problem(s).", file=sys.stderr)
        return 1
    print(f"{len(metadata['packages'])} packages checked against the licence policy.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
