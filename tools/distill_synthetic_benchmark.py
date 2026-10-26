#!/usr/bin/env python3
"""Compare distillers on synthetic data only; print counters, never text.

Usage: python3 -B tools/distill_synthetic_benchmark.py BEFORE_BINARY AFTER_BINARY [--batch-archive]
Set TMPDIR to an isolated directory. No model or real Brain home is accessed.
"""
import http.server
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        prompt = request['messages'][0]['content']
        data = json.loads(prompt.split('\nSession data: ', 1)[1])
        self.server.metrics.append((len(prompt.encode()), len(json.dumps(
            data, ensure_ascii=False, separators=(',', ':')).encode())))
        response = json.dumps({'choices': [{'message': {'content': '{"facts":[]}'}}]}).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(response)))
        self.end_headers()
        self.wfile.write(response)


def measure(binary, batch_archive=False):
    with tempfile.TemporaryDirectory(prefix='distill-synthetic-') as scratch:
        home = Path(scratch)
        archive = home / '.brain/archive'
        archive.mkdir(parents=True, mode=0o700)
        db = archive / 'sessions.sqlite'
        with sqlite3.connect(db) as connection:
            connection.executescript('''
                CREATE TABLE archive_meta(key TEXT PRIMARY KEY,value TEXT);
                INSERT INTO archive_meta VALUES('schema','brain_session_archive:3');
                CREATE TABLE sources(id INTEGER PRIMARY KEY,client TEXT,path TEXT,path_sha256 TEXT);
                INSERT INTO sources VALUES(1,'codex','/synthetic','sha');
                CREATE TABLE entries(id INTEGER PRIMARY KEY,source_id INTEGER,session_id TEXT,role TEXT,
                    locator TEXT,source_offset INTEGER,ts TEXT,content TEXT,content_sha256 TEXT,
                    evidence_sha256 TEXT,record_sha256 TEXT,imported_at TEXT,cwd TEXT);
                CREATE VIRTUAL TABLE entries_fts USING fts5(content,content='entries',content_rowid='id');
                CREATE TABLE cursors(source_id INTEGER PRIMARY KEY,client TEXT,state TEXT);
                CREATE TABLE collector_runs(id INTEGER PRIMARY KEY,started_at TEXT,finished_at TEXT,outcome TEXT,summary TEXT);
                CREATE TABLE quarantine(entry_id INTEGER,reason TEXT,released_at TEXT);
            ''')
            # 30 alternating user/assistant entries of 640 UTF-8 bytes,
            # followed by 100 program outputs of 4096 bytes each.
            for index in range(130):
                role = ('user' if index % 2 == 0 else 'assistant') if index < 30 else 'tool_result'
                text = ('Решение plan. ' * 100).encode()[:640].decode() if index < 30 else 'x' * 4096
                connection.execute('INSERT INTO entries VALUES(?,1,?,?,\'row\',0,\'1790851200\',?,\'sha\',\'sha\',\'sha\',\'2026-10-01\',\'/synthetic\')',
                                   (index + 1, 'synthetic', role, text))
        if batch_archive:
            with sqlite3.connect(db) as connection:
                connection.execute('DELETE FROM entries')
                for index in range(103):
                    text = 's' * 256 if index < 100 else 'L"\\\n' * 15000
                    connection.execute(
                        "INSERT INTO entries VALUES(?,1,?,'assistant','row',0,?,?,'sha','sha','sha','2026-10-01','/synthetic')",
                        (index + 1, f'synthetic-{index:03}', str(1790851200 + index), text))
        db.chmod(0o600)
        (home / '.brain/settings.json').write_text(json.dumps({
            'version': 1, 'profile': 'balanced', 'embedding_mode': 'off'}))
        server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        server.metrics = []
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        env = {k: v for k, v in os.environ.items() if not k.startswith('BRAIN_')}
        env['HOME'] = str(home)

        def cli(*args):
            result = subprocess.run([str(Path(binary).resolve()), '--offline', *args],
                                    env=env, capture_output=True, check=False)
            if result.returncode:
                raise RuntimeError('synthetic benchmark command failed (output suppressed)')
            return json.loads(result.stdout)

        try:
            cli('llm', 'add', 'synthetic', '--base-url',
                f'http://127.0.0.1:{server.server_port}', '--model', 'synthetic',
                '--context-tokens', '16384')
            cli('distill', 'run', '--max-sessions', '1000')
            prompts, payloads = zip(*server.metrics)
            return {'calls': len(prompts), 'payload_bytes_total': sum(payloads),
                    'payload_bytes_min': min(payloads), 'payload_bytes_max': max(payloads),
                    'prompt_bytes_total': sum(prompts), 'prompt_bytes_max': max(prompts)}
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == '__main__':
    if len(sys.argv) not in (3, 4) or (len(sys.argv) == 4 and sys.argv[3] != '--batch-archive'):
        raise SystemExit('usage: distill_synthetic_benchmark.py BEFORE_BINARY AFTER_BINARY [--batch-archive]')
    print(json.dumps({'before': measure(sys.argv[1], len(sys.argv) == 4), 'after': measure(sys.argv[2], len(sys.argv) == 4)}, indent=2))
