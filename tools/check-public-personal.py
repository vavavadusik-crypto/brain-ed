#!/usr/bin/env python3
"""Offline public-tree privacy gate. Reports locations/rules, never values.

Only test directories, test_*.py and Rust #[cfg(test)] modules are fixtures.
Home paths and overlay addresses in those fixtures are synthetic test inputs;
stand identifiers remain forbidden everywhere. No blanket source/doc exemption.
"""
import argparse
import ipaddress
import json
from pathlib import Path
import re
import subprocess
import socket


def rust_test_spans(text):
    # Lex strings/comments before counting braces, including Rust raw strings.
    token = re.compile(r'//[^\n]*|/\*|r(\#*)"|"|\'(?:\\.|[^\'\\])\'|[{}]', re.S)
    spans = []
    for module in re.finditer(r'#\[cfg\((?:test|all\(test,\s*unix\))\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{', text):
        depth, pos = 1, module.end()
        while depth:
            match = token.search(text, pos)
            if not match:
                raise ValueError('unterminated Rust fixture module')
            value, pos = match.group(), match.end()
            if value == '/*':
                nesting = 1
                while nesting:
                    comment = re.search(r'/\*|\*/', text[pos:])
                    if not comment:
                        raise ValueError('unterminated Rust comment')
                    nesting += 1 if comment.group() == '/*' else -1
                    pos += comment.end()
            elif value.startswith('r'):
                end = text.find('"' + match.group(1), pos)
                if end < 0:
                    raise ValueError('unterminated Rust raw string')
                pos = end + 1 + len(match.group(1))
            elif value == '"':
                end = re.search(r'(?:\\.|[^"\\])*"', text[pos:])
                if not end:
                    raise ValueError('unterminated Rust string')
                pos += end.end()
            elif value == '{':
                depth += 1
            elif value == '}':
                depth -= 1
        spans.append((module.start(), pos))
    return spans


def scan(root, stand_names=()):
    paths = subprocess.check_output(
        ['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'], cwd=root
    ).split(b'\0')
    findings = []
    home = re.compile('/' + 'home/')
    overlay = re.compile(r'(?<![\w.])100\.\d{1,3}\.\d{1,3}\.\d{1,3}(?![\d.])|(?i:fd7a):[0-9a-fA-F:]*')
    # Known lab identifiers plus caller-supplied exact names (not regexes).
    stand = re.compile(r'(?i)(?<![\w-])(?:' + '|'.join(re.escape(x) for x in
                       ('KING' + 'STON', 'ai-dev' + '-station', socket.gethostname(), *stand_names)) + r')(?![\w-])')
    for raw in sorted(set(paths)):
        if not raw:
            continue
        rel = raw.decode('utf-8')
        path = root / rel
        # Symlink targets are data too; never follow a link outside the export.
        data = str(path.readlink()).encode() if path.is_symlink() else path.read_bytes()
        text = data.decode('utf-8', errors='replace')
        fixture = (rel.startswith(('tests/', 'brain-cli/tests/', 'brain-core/tests/'))
                   or (Path(rel).name.startswith('test_') and rel.endswith('.py')))
        spans = rust_test_spans(text) if rel.endswith('.rs') else []
        offset = 0
        for number, line in enumerate(text.splitlines(keepends=True), 1):
            in_fixture = fixture or any(start <= offset < end for start, end in spans)
            kinds = set()
            if stand.search(line):
                kinds.add('stand-identifier')
            if not in_fixture:
                if home.search(line):
                    kinds.add('home-path')
                for match in overlay.finditer(line):
                    value = match.group()
                    if value.lower().startswith('fd7a' + ':'):
                        kinds.add('overlay-ipv6')
                    else:
                        try:
                            if ipaddress.ip_address(value) in ipaddress.ip_network((1681915904, 10)):
                                kinds.add('overlay-ipv4')
                        except ValueError:
                            pass
            findings.extend(dict(path=rel, line=number, rule=kind) for kind in sorted(kinds))
            offset += len(line)
    return dict(files=len(set(paths) - {b''}), findings=findings)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--stand-name', action='append', default=[])
    args = parser.parse_args()
    report = scan(args.root, args.stand_name)
    print(json.dumps(report, indent=2))
    return bool(report['findings'])


if __name__ == '__main__':
    raise SystemExit(main())
