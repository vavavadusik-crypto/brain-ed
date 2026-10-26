#!/usr/bin/env python3
"""Isolated E5.9 planner benchmark. Synthetic data only; no model/network calls.

Usage: python3 -B tools/distill_planner_benchmark.py BEFORE_BINARY AFTER_BINARY
Every binary gets the same deterministic archive in a private temporary HOME.
Only counters and elapsed time are printed. No real Brain home is accessed.
"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time


def generate(home):
    archive = home / '.brain/archive'
    archive.mkdir(parents=True, mode=0o700)
    db = archive / 'sessions.sqlite'
    with sqlite3.connect(db) as conn:
        conn.executescript('''
            CREATE TABLE archive_meta(key TEXT PRIMARY KEY,value TEXT);
            INSERT INTO archive_meta VALUES('schema','brain_session_archive:3');
            CREATE TABLE sources(id INTEGER PRIMARY KEY,client TEXT,path TEXT,path_sha256 TEXT);
            INSERT INTO sources VALUES(1,'codex','/synthetic','sha');
            CREATE TABLE entries(id INTEGER PRIMARY KEY,source_id INTEGER,session_id TEXT,role TEXT,
                locator TEXT,source_offset INTEGER,ts TEXT,content TEXT,content_sha256 TEXT,
                evidence_sha256 TEXT,record_sha256 TEXT,imported_at TEXT,cwd TEXT);
            CREATE INDEX entries_session ON entries(source_id,session_id,id);
            CREATE VIRTUAL TABLE entries_fts USING fts5(content,content='entries',content_rowid='id');
            CREATE TABLE cursors(source_id INTEGER PRIMARY KEY,client TEXT,state TEXT);
            CREATE TABLE collector_runs(id INTEGER PRIMARY KEY,started_at TEXT,finished_at TEXT,outcome TEXT,summary TEXT);
            CREATE TABLE quarantine(entry_id INTEGER,reason TEXT,released_at TEXT);
        ''')
        # 889 sessions, 31,115 selected entries, 27.4 MB of content (exceeds the reported 22 MB archive).
        # Mixed UTF-8/JSON escaping exercises the exact wire-size budget.
        text = ('synthetic решение "plan" \\ trace\n' * 22)
        for session in range(889):
            for entry in range(35):
                ident = session * 35 + entry + 1
                role = ('user', 'assistant', 'tool_error')[entry % 3]
                conn.execute('INSERT INTO entries VALUES(?,1,?,?,\'row\',0,?,?,\'sha\',\'sha\',\'sha\',\'2026-10-01\',\'/synthetic\')',
                             (ident, f'synthetic-{session:04}', role,
                              str(1790851200 + ident), text))
    db.chmod(0o600)
    (home / '.brain/settings.json').write_text(json.dumps({
        'version': 1, 'profile': 'balanced', 'embedding_mode': 'off'}))


def measure(binary, extra=(), timeout=900):
    binary = str(Path(binary).resolve())
    with tempfile.TemporaryDirectory(prefix='distill-planner-') as scratch:
        home = Path(scratch)
        generate(home)
        env = {k: v for k, v in os.environ.items() if not k.startswith('BRAIN_')}
        env['HOME'] = str(home)
        env['XDG_CONFIG_HOME'] = str(home / '.config')
        def cli(*args):
            result = subprocess.run([binary, '--offline', *args], env=env,
                                    capture_output=True, timeout=timeout, check=False)
            if result.returncode:
                raise RuntimeError('synthetic planner failed (output suppressed)')
            return json.loads(result.stdout)
        cli('llm', 'add', 'synthetic', '--base-url', 'http://127.0.0.1:1',
            '--model', 'synthetic', '--context-tokens', '16384')
        start = time.monotonic()
        plan = cli('distill', 'run', '--dry-run', '--max-sessions', '1000', *extra)
        elapsed = time.monotonic() - start
        assert not (home / '.brain/brain.sqlite').exists()
        return {'seconds': round(elapsed, 4), 'plan': plan}


if __name__ == '__main__':
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    before, after = (measure(binary) for binary in sys.argv[1:])
    assert before['plan'] == after['plan'], 'planner output changed'
    print(json.dumps({'before': before, 'after': after}, indent=2))
