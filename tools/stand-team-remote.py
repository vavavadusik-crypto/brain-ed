#!/usr/bin/env python3
"""Private stand helper, copied only into the newly created synthetic HOME."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time

root = Path(__file__).resolve().parent
os.chdir(root)
# Never inherit provider, model, Git or Brain settings from the remote account.
os.environ.clear()
os.environ.update(HOME=str(root), BRAIN_HOME=str(root / '.brain'), PATH='/usr/bin:/bin',
                  LANG='C.UTF-8', BRAIN_OFFLINE='true', GIT_CONFIG_NOSYSTEM='1',
                  GIT_CONFIG_GLOBAL='/dev/null', GIT_TEMPLATE_DIR='')
os.umask(0o077)
op, *args = sys.argv[1:]
if op == 'seed':
    import importlib.util
    spec = importlib.util.spec_from_file_location('demo', root / 'team-demo.py')
    demo = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(demo)
    demo.Demo(str(root / 'brain'), root).seed(root, int(args[0]))
    print('{}')
elif op == 'port':
    ip, requested = args
    for port in ([int(requested)] if requested != '0' else range(7450, 7500)):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            try:
                sock.bind((ip, port))
            except OSError:
                continue
        print(port)
        break
    else:
        raise RuntimeError('no free stand port')
elif op == 'serve':
    import shlex
    ip, port, *flags = args
    command = shlex.join([str(root / 'brain'), '--offline', 'net', 'serve',
                          '--state-dir', str(root / '.brain/net'), '--listen', ip,
                          '--port', port, *flags])
    with (root / 'serve.log').open('w') as log:
        proc = subprocess.Popen(['nohup', 'sh', '-c', 'sleep 1800 | ' + command],
                                stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True)
    (root / 'serve.pid').write_text(str(proc.pid))
    for _ in range(100):
        lines = (root / 'serve.log').read_text().splitlines()
        if lines:
            ready = json.loads(lines[0])
            if ready.get('status') != 'serving':
                raise RuntimeError('server did not become ready')
            print('{}')
            break
        if proc.poll() is not None:
            raise RuntimeError('server exited before readiness')
        time.sleep(0.1)
    else:
        raise RuntimeError('server readiness timed out')
elif op == 'stop':
    pidfile = root / 'serve.pid'
    if pidfile.exists():
        pid = int(pidfile.read_text())
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        pidfile.unlink()
    print('{}')
else:
    raise RuntimeError('unknown helper operation')
