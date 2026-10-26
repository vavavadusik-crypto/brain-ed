#!/usr/bin/env python3
"""Compare prepared corpora and run the fixed matrix; never export result text.

--base and --sections are prepared HOME directories. --bins contains live,
morph, sections, combined and snowball. All output must live outside the repo;
only the JSONL scores and manifest are suitable for copying into git.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def parents(home):
    path = home / '.brain/brain.sqlite'
    with sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True) as db:
        db.execute('PRAGMA query_only=ON')
        return {source: hashlib.sha256(body.encode()).hexdigest()
                for source, body in db.execute('''SELECT source, body FROM node_versions
                    WHERE superseded_at IS NULL AND space_id='default'
                    AND json_extract(metadata,'$.imported_from')='markdown'
                    AND json_extract(metadata,'$.section') IS NULL''')}


def section_count(home):
    path = home / '.brain/brain.sqlite'
    with sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True) as db:
        return db.execute('''SELECT count(*) FROM node_versions WHERE superseded_at IS NULL
            AND space_id='default'
            AND json_extract(metadata,'$.imported_from')='markdown'
            AND json_extract(metadata,'$.section') IS NOT NULL''').fetchone()[0]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('base', 'sections', 'bins', 'output'):
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--resume', action='store_true',
                   help='Reuse complete verified scores; incomplete modes get new HOME directories')
    args = p.parse_args()
    args.output.mkdir(mode=0o700, parents=True, exist_ok=args.resume)
    names = ('live', 'morph', 'snowball', 'sections', 'combined')
    binary_hashes = {name: digest(args.bins/name) for name in names}
    if len(set(binary_hashes.values())) != len(names):
        print(json.dumps({'errors': 1, 'duplicate_binaries': 1}))
        return 1
    left, right = parents(args.base), parents(args.sections)
    if not left or left != right:
        print(json.dumps({'errors': 1, 'different_parent_sources': len(set(left) ^ set(right)),
                          'different_parent_bodies': sum(left[k] != right[k] for k in left.keys() & right.keys())}))
        return 1
    for relative in ('settings.json', 'archive/sessions.sqlite'):
        if digest(args.base / '.brain' / relative) != digest(args.sections / '.brain' / relative):
            print(json.dumps({'errors': 1}))
            return 1
    scripts = Path(__file__).resolve().parent
    counts = {name: section_count(home) for name, home in
              (('base', args.base), ('sections', args.sections))}
    if counts['sections'] <= counts['base']:
        print(json.dumps({'errors': 1, 'missing_section_reload': 1}))
        return 1
    manifest = {'parent_documents': len(left), 'section_nodes': counts,
                'parent_content_sha256': hashlib.sha256(json.dumps(left, sort_keys=True).encode()).hexdigest(),
                'questions_sha256': digest(scripts / 'questions.jsonl'),
                'runner_sha256': digest(scripts / 'run.py'),
                'settings_sha256': digest(args.base / '.brain/settings.json'),
                'archive_sha256': digest(args.base / '.brain/archive/sessions.sqlite'),
                'variants': {}}
    previous = {}
    if args.resume:
        previous = json.loads((args.output/'manifest.json').read_text())
        if any(previous.get(k) != v for k, v in manifest.items() if k != 'variants'):
            raise ValueError('resume inputs changed')
        for name, variant in previous['variants'].items():
            fixture = args.sections if name in ('sections', 'combined') else args.base
            if (variant['binary_sha256'] != binary_hashes[name]
                    or variant['database_sha256'] != digest(fixture/'.brain/brain.sqlite')):
                raise ValueError('resume variant changed')

    def run(name, archived):
        home = args.output / (name + ('-archive' if archived else '-core'))
        mode = 'archive' if archived else 'core'
        old = previous.get('variants', {}).get(name, {})
        if mode in old.get('scores', {}):
            score = old['scores'][mode]
            evidence = args.output / old.get('score_files', {}).get(mode, home.name + '.jsonl')
            rows = [json.loads(line) for line in evidence.read_text().splitlines()]
            ids = [json.loads(line)['id'] for line in (scripts/'questions.jsonl').read_text().splitlines()]
            if (len(rows) != len(ids) + 1 or rows[-1] != score or score.get('errors')
                    or [row.get('id') for row in rows[:-1]] != ids
                    or any(row.get('error') for row in rows[:-1])):
                raise ValueError('incomplete resume evidence')
            return score, evidence.name
        original = home
        attempt = 0
        while home.exists() or home.with_suffix('.jsonl').exists():
            attempt += 1
            home = original.with_name(original.name + '-retry' + str(attempt))
        fixture = args.sections if name in ('sections', 'combined') else args.base
        brain = fixture / '.brain'
        prepare = subprocess.run([sys.executable, str(scripts/'prepare.py'), '--home', str(home),
                                  '--snapshot', str(brain), '--settings', str(brain/'settings.json'),
                                  '--models', str(brain/'models')], capture_output=True)
        if prepare.returncode:
            return {'errors': 1}, home.with_suffix('.jsonl').name
        command = [sys.executable, str(scripts/'run.py'), str(args.bins/name), '--home', str(home)]
        if archived:
            command.append('--with-archive')
        with home.with_suffix('.jsonl').open('x') as output:
            result = subprocess.run(command, stdout=output, stderr=subprocess.DEVNULL)
        summary = json.loads(home.with_suffix('.jsonl').read_text().splitlines()[-1])
        if result.returncode:
            summary['errors'] = max(1, summary.get('errors', 0))
        return summary, home.with_suffix('.jsonl').name

    # Bound memory pressure: two CLI processes, each with its own HOME.
    with ThreadPoolExecutor(max_workers=2) as pool:
        for name in names:
            fixture = args.sections if name in ('sections', 'combined') else args.base
            futures = {mode: pool.submit(run, name, mode == 'archive') for mode in ('core', 'archive')}
            completed = {mode: future.result() for mode, future in futures.items()}
            scores = {mode: result[0] for mode, result in completed.items()}
            manifest['variants'][name] = {'binary_sha256': digest(args.bins/name),
                                          'database_sha256': digest(fixture/'.brain/brain.sqlite'),
                                          'score_files': {mode: result[1] for mode, result in completed.items()},
                                          'scores': scores}
            checkpoint = dict(manifest, variants={**previous.get('variants', {}), **manifest['variants']})
            temporary = args.output/'manifest.tmp'
            temporary.write_text(json.dumps(checkpoint, indent=2)+'\n')
            temporary.replace(args.output/'manifest.json')
            print(json.dumps(scores), flush=True)
            if any(score.get('errors') for score in scores.values()):
                return 1
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception:
        print(json.dumps({'errors': 1}))
        sys.exit(1)
