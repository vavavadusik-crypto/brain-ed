#!/usr/bin/env python3
"""Source/section judgments only. Never emit retrieved text or subprocess stderr."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sqlite3
import sys
import time


def heading(value):
    # Station memory embeds the answer after '|'; export only its heading title.
    return re.sub(r'\s+', ' ', value.lstrip('#').strip().split(' | ', 1)[0]).casefold()


def path_key(value):
    return str(Path(value).expanduser().resolve())


def headings(text):
    result = set()
    fence = None
    for line in text.splitlines():
        match = re.match(r'^\s*(`{3,}|~{3,})', line)
        if match:
            marker = match.group(1)
            if fence is None:
                fence = marker
            elif marker[0] == fence[0] and len(marker) >= len(fence):
                fence = None
            continue
        if fence is None and re.match(r'^#{1,6}\s', line):
            result.add(heading(line))
    return result


def relevant(hit, expected):
    if 'client' in hit:
        return False
    if not isinstance(hit.get('source'), str):
        return False
    if path_key(hit['source']) != path_key(expected['path']):
        return False
    section = expected.get('section')
    if not section:
        return True
    # Whole documents count only if the returned body actually contains the
    # judged section. Sibling sections from the same file do not count.
    label = re.sub(r' \(part \d+/\d+\)$', '', hit.get('_section_label', ''))
    return heading(section) in headings(hit.get('text', '')) or (
        bool(label) and heading(label) == heading(section))


def attach_section_labels(hits, database):
    # Ask exposes source and ID but omits section labels. Read only these
    # identifiers locally so continuation chunks without a heading still count.
    with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as db:
        db.execute('PRAGMA query_only=ON')
        for hit in hits[:10]:
            row = db.execute("""SELECT label, source, metadata FROM node_versions
                WHERE space_id='default' AND node_id=? AND superseded_at IS NULL
                ORDER BY revision DESC LIMIT 1""", (hit.get('id'),)).fetchone()
            if row and row[1] == hit.get('source'):
                metadata = json.loads(row[2])
                if metadata.get('section') is not None:
                    hit['_section_label'] = row[0]


def metrics(hits, expected):
    seen = set()
    gains = []
    for hit in hits[:10]:
        matched = {i for i, item in enumerate(expected) if relevant(hit, item)}
        fresh = matched - seen
        gains.append(int(bool(fresh)))
        seen |= matched
    recalled = {i for hit in hits[:5] for i, item in enumerate(expected)
                if relevant(hit, item)}
    first = next((i for i, gain in enumerate(gains, 1) if gain), 0)
    dcg = sum(gain / math.log2(i + 1) for i, gain in enumerate(gains, 1))
    ideal = sum(1 / math.log2(i + 1) for i in range(1, min(10, len(expected)) + 1))
    return {'recall_at_5': len(recalled) / len(expected),
            'hit_at_5': int(bool(recalled)), 'mrr_at_10': 1 / first if first else 0,
            'ndcg_at_10': dcg / ideal, 'first_rank': first}


def validate(rows):
    if not rows or len({r['id'] for r in rows}) != len(rows):
        raise ValueError('invalid questions')
    for row in rows:
        if not re.fullmatch(r'Q[0-9]+', row['id']):
            raise ValueError('invalid id')
        if not row['question'] or not 1 <= len(row['expected']) <= 3:
            raise ValueError('invalid judgment')
        if len({(e['path'], e.get('section')) for e in row['expected']}) != len(row['expected']):
            raise ValueError('duplicate judgment')


def run(args):
    home = args.home.resolve(strict=True)
    protected = Path.home().resolve()
    if home == protected or any(home.is_relative_to(protected / p)
                               for p in ('.brain', '.hermes', '.claude', '.codex')):
        raise ValueError('protected home')
    brain = home / '.brain'
    if brain.is_symlink() or not (brain / 'brain.sqlite').is_file():
        raise ValueError('invalid home')
    for p in brain.rglob('*'):
        if p.is_symlink() and p.name != 'models':
            raise ValueError('unsafe symlink')
        if p.is_file() and not p.is_symlink() and p.stat().st_nlink != 1:
            raise ValueError('unsafe hard link')
    rows = [json.loads(line) for line in args.questions.read_text().splitlines() if line]
    validate(rows)
    # One (variant, mode) run per HOME; even a failed run consumes it.
    with (brain / '.retrieval-bench-used').open('x') as marker:
        marker.write(hashlib.sha256(args.questions.read_bytes()).hexdigest())
    env = dict(os.environ, HOME=str(home), BRAIN_OFFLINE='true',
               BRAIN_DATABASE=str(brain / 'brain.sqlite'),
               BRAIN_MODELS_DIR=str(brain / 'models'), BRAIN_AGENT='retrieval-bench',
               XDG_CONFIG_HOME=str(home / '.config'), XDG_CACHE_HOME=str(home / '.cache'),
               XDG_DATA_HOME=str(home / '.local/share'))
    scores = []
    for row in rows:
        cmd = [str(args.binary.resolve()), 'ask', row['question'],
               '--budget-tokens', str(args.budget_tokens)]
        if not args.with_archive:
            cmd.append('--no-archive')
        start = time.monotonic()
        error = 0
        vectors = warnings = 0
        try:
            result = subprocess.run(cmd, env=env, capture_output=True, timeout=args.timeout)
            if result.returncode:
                raise ValueError('command failed')
            payload = json.loads(result.stdout)
            if not isinstance(payload.get('results'), list):
                raise ValueError('invalid output')
            if bool(payload.get('archive_included')) != args.with_archive:
                raise ValueError('wrong archive mode')
            attach_section_labels(payload['results'], brain / 'brain.sqlite')
            score = metrics(payload['results'], row['expected'])
            vectors = int(bool(payload.get('vector_search')))
            warnings = len(payload.get('warnings', []))
        except (ValueError, OSError, TypeError, sqlite3.Error, subprocess.TimeoutExpired):
            error = 1
            score = metrics([], row['expected'])
        score.update(id=row['id'], error=error, vector_search=vectors, warnings=warnings,
                     seconds=round(time.monotonic()-start, 6))
        scores.append(score)
        print(json.dumps(score), flush=True)
    summary = {key: sum(s[key] for s in scores) / len(scores)
               for key in ('recall_at_5', 'mrr_at_10', 'ndcg_at_10')}
    summary.update(questions=len(scores), hits_at_5=sum(s['hit_at_5'] for s in scores),
                   errors=sum(s['error'] for s in scores),
                   vector_queries=sum(s['vector_search'] for s in scores),
                   warnings=sum(s['warnings'] for s in scores),
                   seconds=sum(s['seconds'] for s in scores))
    print(json.dumps(summary), flush=True)
    return int(bool(summary['errors']))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--home', type=Path, required=True)
    parser.add_argument('--questions', type=Path, default=Path(__file__).with_name('questions.jsonl'))
    parser.add_argument('--with-archive', action='store_true')
    parser.add_argument('--budget-tokens', type=int, default=12000)
    parser.add_argument('--timeout', type=float, default=120)
    args = parser.parse_args()
    try:
        return run(args)
    except Exception:
        # No tracebacks: errors may contain private payloads or paths.
        print(json.dumps({'errors': 1}))
        return 1


if __name__ == '__main__':
    sys.exit(main())
