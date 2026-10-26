#!/usr/bin/env python3
"""Paired archive/core bench + external exam, fresh HOME per run, numbers only.

Prepare one SQLite .backup snapshot first; all variants must use that snapshot.
No retrieved text, source names, dates, or subprocess stderr is persisted.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from types import SimpleNamespace

from prepare import prepare


def digest(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def exam_scores(output):
    rows = []
    for line in output.splitlines():
        match = re.match(r'^(E[0-9]+): (hit|miss|ERROR)\b', line)
        if match:
            rows.append({'id': match[1], 'hit': int(match[2] == 'hit'),
                         'error': int(match[2] == 'ERROR')})
    if [row['id'] for row in rows] != [f'E{i}' for i in range(1, 11)]:
        raise ValueError('incomplete exam')
    return {'questions': len(rows), 'hits': sum(row['hit'] for row in rows),
            'errors': sum(row['error'] for row in rows), 'rows': rows}


def measure(args):
    # prepare() rejects protected homes and any existing run directory.
    args.output.mkdir(parents=True, exist_ok=False)
    args.homes.mkdir(parents=True, exist_ok=False)
    inputs = {
        'binary_sha256': digest(args.binary),
        'database_sha256': digest(args.snapshot / 'brain.sqlite'),
        'archive_sha256': digest(args.snapshot / 'archive/sessions.sqlite'),
        'settings_sha256': digest(args.settings),
        'exam_sha256': digest(args.exam),
        'questions_sha256': digest(Path(__file__).with_name('questions.jsonl')),
        'runner_sha256': digest(Path(__file__).with_name('run.py')),
        'measurement_sha256': digest(Path(__file__)),
        'budget_tokens': 12000,
    }
    report = {'inputs': inputs, 'exam': {}, 'bench': {}}
    for suite in ('exam', 'bench'):
        for mode in ('core', 'archive'):
            home = args.homes / f'{suite}-{mode}'
            prepare(SimpleNamespace(home=home, snapshot=args.snapshot, settings=args.settings,
                                    models=args.models, reload=False))
            env = dict(os.environ, HOME=str(home), BRAIN_OFFLINE='true',
                       BRAIN_DATABASE=str(home / '.brain/brain.sqlite'),
                       BRAIN_MODELS_DIR=str(home / '.brain/models'),
                       BRAIN_AGENT='retrieval-bench',
                       XDG_CONFIG_HOME=str(home / '.config'),
                       XDG_CACHE_HOME=str(home / '.cache'),
                       XDG_DATA_HOME=str(home / '.local/share'))
            # Prevent accidental reuse even if the external exam exits early.
            (home / '.brain/.archive-rank-measurement-used').touch(exist_ok=False)
            if suite == 'exam':
                command = [sys.executable, str(args.exam), str(args.binary), '12000']
                if mode == 'core':
                    command.append('--no-archive')
            else:
                command = [sys.executable, str(Path(__file__).with_name('run.py')),
                           str(args.binary), '--home', str(home)]
                if mode == 'archive':
                    command.append('--with-archive')
            result = subprocess.run(command, env=env, capture_output=True, text=True,
                                    timeout=1200)
            if result.returncode:
                raise ValueError('measurement subprocess failed')
            if suite == 'exam':
                score = exam_scores(result.stdout)
            else:
                rows = [json.loads(line) for line in result.stdout.splitlines()]
                if len(rows) != 51 or rows[-1].get('errors') != 0:
                    raise ValueError('incomplete benchmark')
                score = rows[-1]
                # The bench runner emits only numerical measurements and Q IDs.
                for row in rows:
                    if any(not isinstance(value, (int, float)) and
                           not (key == 'id' and re.fullmatch(r'Q[0-9]+', value))
                           for key, value in row.items()):
                        raise ValueError('unexpected benchmark field')
                (args.output / f'bench-{mode}.jsonl').write_text(
                    ''.join(json.dumps(row) + '\n' for row in rows))
            report[suite][mode] = score
            (args.output / 'summary.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps({'suite': suite, 'mode': mode, 'score': score}), flush=True)
    # Source snapshot must remain byte-identical throughout the measurements.
    if (digest(args.snapshot / 'brain.sqlite') != inputs['database_sha256'] or
            digest(args.snapshot / 'archive/sessions.sqlite') != inputs['archive_sha256']):
        raise ValueError('snapshot changed')
    return int(any(score['errors'] for suite in ('exam', 'bench')
                   for score in report[suite].values()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'snapshot', 'settings', 'models', 'exam', 'homes', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    for name, value in vars(args).items():
        setattr(args, name, value.absolute())
    try:
        return measure(args)
    except Exception:
        # A failed child may contain private output; never print it or a traceback.
        print(json.dumps({'errors': 1}), flush=True)
        return 1


if __name__ == '__main__':
    sys.exit(main())
