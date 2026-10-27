"""Offline process tests: PATH wrappers replace every SSH/SCP invocation."""
import importlib.util
import json
import os
import shutil
import socket
from unittest.mock import patch
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
FAKE = r'''#!/usr/bin/env python3
import json, os, pathlib, shlex, shutil, signal, subprocess, sys
BASE = pathlib.Path(__BASE__)
args = sys.argv[1:]
role = pathlib.Path(sys.argv[0]).name
with (BASE/'events').open('a') as f:
    f.write(json.dumps([role, args])+'\n')
def read(root):
    p=root/'state.json'
    return json.loads(p.read_text()) if p.exists() else {}
def write(root, state):
    (root/'state.json').write_text(json.dumps(state))
def option(key):
    return args[args.index(key)+1]
if role == 'file':
    print('ELF 64-bit LSB executable, '+ ('aarch64' if 'arm' in args[-1] else 'x86-64') + ', statically linked')
elif role == 'scp':
    def path(s):
        return pathlib.Path(s.split(':',1)[1] if ':' in s else s)
    src, dst = map(path,args[-2:])
    if src.suffix in ('.bundle','.credential','.reader'):
        assert src.stat().st_mode & 0o077 == 0
    if os.environ.get('FAIL') == 'partial' and src.name == 'stand-team-remote.py':
        sys.exit(7)
    shutil.copyfile(src,dst)
    dst.chmod(0o600)
    if os.environ.get('FAIL') == 'signal' and src.suffix == '.bundle' and not (BASE/'signalled').exists():
        (BASE/'signalled').touch()
        os.kill(os.getppid(), signal.SIGTERM)
    if os.environ.get('FAIL') == 'scp' and src.suffix == '.bundle':
        print('TOP_SECRET_TOKEN',file=sys.stderr)
        sys.exit(7)
elif role == 'ssh':
    host, command=args[-2:]
    home=BASE/'hosts'/host
    home.mkdir(parents=True,exist_ok=True)
    if command == 'uname -m':
        print('aarch64' if host == 'arm' else 'x86_64')
    elif command == 'tailscale ip -4':
        if os.environ.get('FAIL') == 'ip': sys.exit(4)
        print('192.168.1.4' if host == 'arm' else '100.64.0.4')
    elif '/stand-team-remote.py' in command:
        tokens=shlex.split(command)
        index=next(i for i,s in enumerate(tokens) if s.endswith('/stand-team-remote.py'))
        root=pathlib.Path(tokens[index]).parent
        op=tokens[index+1].rstrip(';')
        state=read(root)
        if op=='port': print('7451')
        else:
            state[op]=True
            if op=='serve': state['online']=True
            if op=='stop': state['online']=False
            write(root,state)
            print('{}')
    else:
        env=dict(os.environ,HOME=str(home))
        sys.exit(subprocess.run(['sh','-c',command],env=env,capture_output=False).returncode)
else:
    root=pathlib.Path(os.environ['HOME'])
    assert os.environ['BRAIN_HOME']==str(root/'.brain')
    assert root.name.startswith('brain-stand-')
    assert 'POISON_PROVIDER' not in os.environ
    args=args[1:] # --offline
    state=read(root)
    out={}
    if args[:2]==['net','enroll']:
        for flag in ['--credential-out','--bundle-out']:
            if flag in args:
                p=pathlib.Path(option(flag)); p.write_text('TOP_SECRET_TOKEN'); p.chmod(0o600)
    if args[:2]==['net','join']:
        bundle=pathlib.Path(args[2]); assert bundle.read_text()=='TOP_SECRET_TOKEN'
        assert '--consume' in args
        bundle.unlink()
    if args[:2]==['net','init']:
        p=root/'.brain/net/tls'; p.mkdir(parents=True,exist_ok=True); (p/'ca.pem').write_text('CA')
    if args[:2]==['team','configure']:
        state['machine']=option('--machine')
    if args[:2]==['team','manifest']:
        out={'manifests':[{'manifest':{'origin':'origin-'+state['machine']}}],
             'signing_public_key':'a'*64}
    if args[:2]==['team','disable']: state['disabled']=True
    if args[:2]==['team','publish']:
        state['published']=True
        state['disabled_published']=state.get('disabled',False)
    if args[:2]==['team','ask']:
        hits=[]; machines=[]; lines=[]
        for name in option('--machines').split(','):
            sources=[p for p in (BASE/'hosts').glob('*/brain-stand-*') if read(p).get('machine')==name]
            s=read(sources[0]); status='ok'; cls=None
            if args[2]=='backoff': status='offline' if not s.get('online') else 'ok'
            elif args[2]=='reopened': status='empty' if s.get('disabled_published') else 'denied'
            else:
                cls='task' if 'заняты' in args[2] else 'error' if 'ошибки' in args[2] else 'decision'
                hits.append({'received_from_peer':name,'fact':{'content':{'class':cls}}})
            machines.append({'status':status})
            lines.append('машина '+name+' класс '+str(cls)+' '+status)
        out={'hits':hits,'machines':machines,'lines':lines}
    write(root,state)
    print(json.dumps(out))
'''


class StandTeam(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='stand-test-')
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.bin = self.base / 'bin'
        self.bin.mkdir()
        fake = FAKE.replace('__BASE__', repr(str(self.base)))
        for name in ('ssh', 'scp', 'file', 'brain-x86', 'brain-arm'):
            p = self.bin / name
            p.write_text(fake)
            p.chmod(0o700)
        self.env = dict(os.environ, PATH=str(self.bin) + ':' + os.environ['PATH'], POISON_PROVIDER='must-not-inherit')

    def run_stand(self, *extra, failure=None):
        env = dict(self.env)
        if failure:
            env['FAIL'] = failure
        p = subprocess.run([str(REPO / 'tools/stand-team.sh'), '--coordinator', 'boss',
                            '--machines', 'x86,arm', '--bin-x86', str(self.bin / 'brain-x86'),
                            '--bin-arm', str(self.bin / 'brain-arm'), *extra],
                           env=env, text=True, capture_output=True, timeout=30)
        self.assertNotIn('TOP_SECRET_TOKEN', p.stdout + p.stderr)
        return p

    def events(self):
        return [json.loads(line) for line in (self.base / 'events').read_text().splitlines()]

    def test_success_order_isolation_and_cleanup(self):
        p = self.run_stand()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertIn('класс выключен', p.stdout)
        self.assertIn('машина офлайн', p.stdout)
        self.assertIn('aarch64', p.stdout)
        events = self.events()
        commands = '\n'.join(e[1][-1] for e in events if e[0] == 'ssh')
        self.assertLess(commands.index('net enroll'), commands.index('net join'))
        self.assertLess(commands.index('net join'), commands.index('team enable'))
        self.assertLess(commands.index('team enable'), commands.index('team publish'))
        self.assertLess(commands.index('team publish'), commands.index('team ask'))
        self.assertIn('--allow-cgnat-dial', commands)
        self.assertIn('--allow-lan-dial', commands)
        self.assertIn('--ip 100.64.0.4', commands)
        peers = [args[-1] for role, args in events if role == 'ssh' and 'team peer' in args[-1]]
        self.assertEqual(len(peers), 2)
        self.assertTrue(all('--public-key ' + 'a' * 64 in command for command in peers))
        for role, args in events:
            if role in ('ssh', 'scp'):
                self.assertIn('-oBatchMode=yes', args)
                self.assertIn('-oConnectTimeout=8', args)
                self.assertIn('-oUpdateHostKeys=no', args)
        self.assertEqual(list((self.base / 'hosts').glob('*/brain-stand-*')), [])

    def test_keep_stops_sources_and_consumes_bundles(self):
        p = self.run_stand('--keep', '--port', '7488')
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        roots = list((self.base / 'hosts').glob('*/brain-stand-*'))
        self.assertEqual(len(roots), 3)
        for root in roots:
            self.assertFalse(json.loads((root / 'state.json').read_text()).get('online'))
            self.assertEqual(list(root.glob('*.bundle')), [])
        self.assertTrue(any('7488' in e[1][-1] for e in self.events() if e[0] == 'ssh'))

    def test_transfer_failure_cleans_all_created_hosts(self):
        p = self.run_stand(failure='scp')
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(list((self.base / 'hosts').glob('*/brain-stand-*')), [])

    def test_partial_helper_upload_is_cleaned(self):
        p = self.run_stand(failure='partial')
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(list((self.base / 'hosts').glob('*/brain-stand-*')), [])

    def test_sigterm_cleans_created_hosts(self):
        p = self.run_stand(failure='signal')
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(list((self.base / 'hosts').glob('*/brain-stand-*')), [])
        self.assertIn('серверы остановлены', p.stdout)

    def test_ip_failure_is_nonzero_and_cleans(self):
        p = self.run_stand(failure='ip')
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(list((self.base / 'hosts').glob('*/brain-stand-*')), [])

    def test_rejects_shell_injection_before_ssh(self):
        p = self.run_stand('--coordinator', '-oProxyCommand=oops')
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse((self.base / 'events').exists())

    def test_coordinator_ip_override_skips_tailscale_on_coordinator(self):
        p = self.run_stand('--coordinator-ip', '10.0.0.9')
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        commands = [args for role, args in self.events() if role == 'ssh']
        self.assertFalse(any(args[-2:] == ['boss', 'tailscale ip -4'] for args in commands))
        self.assertTrue(any('--ip 10.0.0.9' in args[-1] for args in commands))

    def test_dynamic_and_wrong_architecture_binaries_are_rejected(self):
        spec = importlib.util.spec_from_file_location('stand', REPO / 'tools/stand-team.py')
        stand = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stand)
        for description in ('ELF 64-bit LSB executable, x86-64, dynamically linked',
                            'ELF 64-bit LSB executable, aarch64, statically linked'):
            result = subprocess.CompletedProcess([], 0, stdout=description)
            with patch.object(stand.subprocess, 'run', return_value=result):
                with self.assertRaises(RuntimeError):
                    stand.static_binary(self.bin / 'brain-x86', 'x86_64')

    def test_command_timeout_is_bounded_and_redacted(self):
        spec = importlib.util.spec_from_file_location('stand', REPO / 'tools/stand-team.py')
        stand = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stand)
        scenario = stand.Stand(None, self.base)
        with patch.object(stand.subprocess, 'run', side_effect=subprocess.TimeoutExpired('TOP_SECRET_TOKEN', 45)) as run:
            with self.assertRaisesRegex(RuntimeError, 'таймаут команды') as caught:
                scenario.process(['ssh', 'fake'])
            self.assertNotIn('TOP_SECRET_TOKEN', str(caught.exception))
            self.assertEqual(run.call_args.kwargs['timeout'], 45)

    def test_address_policy(self):
        spec = importlib.util.spec_from_file_location('stand', REPO / 'tools/stand-team.py')
        stand = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stand)
        self.assertEqual(stand.address('100.64.1.1')[1], '--allow-cgnat')
        self.assertEqual(stand.address('10.2.3.4')[1], '--allow-lan')
        for ip in ('8.8.8.8', '127.0.0.1', '169.254.1.1', '::1'):
            with self.assertRaises((ValueError, RuntimeError)):
                stand.address(ip)


@unittest.skipUnless(os.environ.get('BRAIN_PROBE_BIN'), 'set BRAIN_PROBE_BIN for real local helper test')
class LocalHelper(unittest.TestCase):
    def test_seed_busy_port_serve_and_stop(self):
        # Executes the actual helper with a real Brain on loopback, never SSH.
        with tempfile.TemporaryDirectory(prefix='stand-helper-') as directory:
            root = Path(directory)
            for name in ('stand-team-remote.py', 'team-demo.py'):
                shutil.copy2(REPO / 'tools' / name, root / name)
            shutil.copy2(Path(os.environ['BRAIN_PROBE_BIN']).resolve(), root / 'brain')
            env = dict(os.environ, HOME=str(root), BRAIN_HOME=str(root / '.brain'))
            def helper(*args, ok=True):
                result = subprocess.run(['python3', str(root / 'stand-team-remote.py'), *map(str, args)],
                                        capture_output=True, text=True, env=env, timeout=20)
                self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
                return result.stdout
            def brain(*args):
                result = subprocess.run([str(root / 'brain'), '--offline', *args], env=env,
                                        capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            helper('seed', 1)
            self.assertTrue((root / '.brain/brain.sqlite').is_file())
            brain('net', 'init', '--state-dir', str(root / '.brain/net'), '--ip', '127.0.0.1')
            with socket.socket() as busy:
                busy.bind(('127.0.0.1', 0))
                helper('port', '127.0.0.1', busy.getsockname()[1], ok=False)
            port = int(helper('port', '127.0.0.1', 0))
            self.assertIn(port, range(7450, 7500))
            try:
                helper('serve', '127.0.0.1', port)
                with socket.create_connection(('127.0.0.1', port), timeout=2):
                    pass
            finally:
                helper('stop')
            self.assertFalse((root / 'serve.pid').exists())
            helper('stop')


if __name__ == '__main__':
    unittest.main()
