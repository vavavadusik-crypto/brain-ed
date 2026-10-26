#!/usr/bin/env python3
"""Create a private benchmark HOME from a snapshot; report counts only."""
import argparse
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys


def prepare(args):
    home = args.home.absolute()
    owner = Path.home().resolve()
    if home.exists() or home.resolve() == owner or any(
            home.resolve().is_relative_to(owner / p)
            for p in ('.brain', '.hermes', '.claude', '.codex')):
        raise ValueError('home must be new and isolated')
    for database in (args.snapshot / 'brain.sqlite', args.snapshot / 'sessions.sqlite',
                     args.snapshot / 'archive/sessions.sqlite'):
        wal = Path(str(database) + '-wal')
        if wal.exists() and wal.stat().st_size:
            raise ValueError('snapshot has an uncheckpointed WAL')
    os.umask(0o077)
    home.mkdir(parents=True, mode=0o700)
    brain = home / '.brain'
    (brain / 'archive').mkdir(parents=True)
    shutil.copyfile(args.snapshot / 'brain.sqlite', brain / 'brain.sqlite')
    archive = args.snapshot / 'sessions.sqlite'
    if not archive.exists():
        archive = args.snapshot / 'archive/sessions.sqlite'
    shutil.copyfile(archive, brain / 'archive/sessions.sqlite')
    shutil.copyfile(args.settings, brain / 'settings.json')
    (brain / 'models').symlink_to(args.models.resolve(strict=True))
    count = 0
    if args.reload:
        # Force all compared importers to refresh the same Markdown corpus.
        # Changing a cache hash in this disposable copy avoids reusing stale
        # section layouts from earlier importer builds. Original is untouched.
        with sqlite3.connect(brain / 'brain.sqlite') as db:
            count = db.execute("""UPDATE node_versions
                SET metadata=json_set(metadata,'$.content_hash','e33-force-refresh')
                WHERE superseded_at IS NULL
                AND json_extract(metadata,'$.imported_from')='markdown'""").rowcount
        env = dict(os.environ, HOME=str(home), BRAIN_OFFLINE='true',
                   BRAIN_DATABASE=str(brain / 'brain.sqlite'),
                   BRAIN_MODELS_DIR=str(brain / 'models'), BRAIN_AGENT='retrieval-bench',
                   XDG_CACHE_HOME=str(home/'.cache'), XDG_CONFIG_HOME=str(home/'.config'),
                   XDG_DATA_HOME=str(home/'.local/share'))
        result = subprocess.run([str(args.loader), str(args.binary.resolve())], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode:
            raise ValueError('load failed')
    return count


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--home', type=Path, required=True)
    p.add_argument('--snapshot', type=Path, required=True)
    p.add_argument('--settings', type=Path, required=True)
    p.add_argument('--models', type=Path, required=True)
    p.add_argument('--binary', type=Path)
    p.add_argument('--loader', type=Path, help='explicit local corpus loader, required with --reload')
    p.add_argument('--reload', action='store_true')
    args = p.parse_args()
    try:
        if args.reload and (args.binary is None or args.loader is None):
            raise ValueError('reload requires binary and loader')
        print(json.dumps({'prepared': 1, 'invalidated_cache_hashes': prepare(args)}))
        return 0
    except Exception:
        print(json.dumps({'errors': 1}))
        return 1


if __name__ == '__main__':
    sys.exit(main())
