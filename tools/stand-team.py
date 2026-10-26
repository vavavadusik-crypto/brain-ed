#!/usr/bin/env python3
"""Explicit SSH stand; never run automatically against real machines."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import uuid

HERE = Path(__file__).resolve().parent
OPTIONS = ['-oBatchMode=yes', '-oConnectTimeout=8', '-oStrictHostKeyChecking=yes',
           '-oUpdateHostKeys=no', '-oServerAliveInterval=5', '-oServerAliveCountMax=2']


def check(condition, message):
    if not condition:
        raise RuntimeError(message)


def address(value):
    ip = ipaddress.IPv4Address(value)
    if ip in ipaddress.ip_network((1681915904, 10)):
        return str(ip), '--allow-cgnat', '--allow-cgnat-dial'
    if any(ip in ipaddress.ip_network(net) for net in ('10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16')):
        return str(ip), '--allow-lan', '--allow-lan-dial'
    raise RuntimeError('Нужен частный LAN или tailnet IPv4')


def static_binary(path, arch):
    path = Path(path).resolve()
    check(path.is_file(), f'Нет бинарника для {arch}: {path}')
    result = subprocess.run(['file', '-b', str(path)], capture_output=True, text=True, timeout=10)
    description = result.stdout
    check(result.returncode == 0 and ('statically linked' in description or 'static-pie linked' in description),
          f'Нужен статический Linux ELF для {arch}')
    check('ELF 64-bit' in description and ('x86-64' if arch == 'x86_64' else 'aarch64') in description,
          f'Архитектура бинарника не совпадает: {arch}')
    return path


class Stand:
    def __init__(self, args, local):
        self.args, self.local = args, Path(local)
        self.name = 'brain-stand-' + uuid.uuid4().hex[:16]
        self.roots, self.hosts, self.rows = {}, [], []
        self.stage = 'подготовка'

    def process(self, argv):
        try:
            p = subprocess.run(list(map(str, argv)), capture_output=True, text=True, timeout=45)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f'{self.stage}: таймаут команды (45 с)') from None
        # Never include remote stdout/stderr: enroll and failure output can contain credentials.
        check(p.returncode == 0, f'{self.stage}: команда завершилась с кодом {p.returncode}')
        return p.stdout.strip()

    def ssh(self, host, command):
        self.stage = f'{host}: {self.stage.split(": ")[-1]}'
        return self.process(['ssh', *OPTIONS, host, command])

    def copy(self, source, dest):
        self.process(['scp', '-O', '-q', *OPTIONS, source, dest])

    def run(self, host, *args, helper=False):
        root = self.roots[host]
        argv = (['python3', root + '/stand-team-remote.py'] if helper else
                [root + '/brain', '--offline']) + list(map(str, args))
        if not helper and tuple(args[:2]) in (('net', 'init'), ('net', 'enroll'), ('net', 'serve')):
            argv += ['--state-dir', root + '/.brain/net']
        env = ['env', '-i', 'PATH=/usr/bin:/bin', 'LANG=C.UTF-8', 'BRAIN_OFFLINE=true',
               'HOME=' + root, 'BRAIN_HOME=' + root + '/.brain']
        self.stage = ' '.join(map(str, args[:2]))
        output = self.ssh(host, 'umask 077; cd ' + shlex.quote(root) + '; ' + shlex.join(env + argv))
        try:
            return json.loads(output)
        except ValueError:
            raise RuntimeError(f'{host}: неверный JSON на этапе {self.stage}') from None

    def transfer(self, host, remote, target, filename):
        local = self.local / filename
        local.touch(mode=0o600)
        self.copy(host + ':' + remote, local)
        local.chmod(0o600)
        self.copy(local, target + ':' + self.roots[target] + '/' + filename)
        local.unlink()
        return self.roots[target] + '/' + filename

    def prepare(self, host):
        self.stage = 'создание изолированного каталога'
        # Record before creation: cleanup must also cover a lost SSH response.
        self.hosts.append(host)
        command = 'umask 077; mkdir "$HOME/' + self.name + '" && cd "$HOME/' + self.name + '" && pwd -P'
        root = self.ssh(host, command)
        check(root.startswith('/') and re.fullmatch(r'/[A-Za-z0-9_./-]+', root) is not None
              and root.endswith('/' + self.name), f'{host}: небезопасный путь стенда')
        self.roots[host] = root
        self.stage = 'uname -m'
        arch = self.ssh(host, 'uname -m')
        check(arch in ('x86_64', 'aarch64'), f'{host}: неподдерживаемая архитектура (нужны x86_64/aarch64)')
        supplied = self.args.bin_x86 if arch == 'x86_64' else self.args.bin_arm
        check(supplied, f'{host}: задайте --bin-{"x86" if arch == "x86_64" else "arm"}')
        binary = static_binary(supplied, arch)
        for path, name in [(binary, 'brain'), (HERE / 'stand-team-remote.py', 'stand-team-remote.py'),
                           (HERE / 'team-demo.py', 'team-demo.py')]:
            self.copy(path, host + ':' + root + '/' + name)
        self.ssh(host, 'chmod 700 ' + shlex.quote(root + '/brain'))
        return arch

    def ip(self, host, override=None):
        if override:
            return address(override)
        self.stage = 'tailscale ip (для координатора доступен --coordinator-ip)'
        output = self.ssh(host, 'tailscale ip -4')
        try:
            return address(output.splitlines()[0])
        except (ValueError, IndexError):
            raise RuntimeError(f'{host}: tailscale ip не вернул допустимый частный IPv4') from None

    def server(self, host, ip, listen_flag):
        port = self.run(host, 'port', ip, self.args.port or 0, helper=True)
        self.run(host, 'serve', ip, port, listen_flag, helper=True)
        return f'{ip}:{port}'

    def ask(self, machines, question, allowed=None):
        print('\nРуководитель: ' + question, flush=True)
        result = self.run(self.args.coordinator, 'team', 'ask', question,
                          '--machines', ','.join(machines), '--limit', '100', '--deadline-ms', '15000')
        for line in result['lines']:
            print('  ' + line, flush=True)
        if allowed:
            check(result['hits'] and all(h['fact']['content']['class'] in allowed for h in result['hits']),
                  'Неверные классы ответа')
            check({h['received_from_peer'] for h in result['hits']} == set(machines), 'Нет атрибуции всех машин')
            for row in self.rows:
                row[4] += sum(h['received_from_peer'] == row[0] for h in result['hits'])
        return result

    def execute(self):
        boss = self.args.coordinator
        self.prepare(boss)
        ip, listen, dialflag = self.ip(boss, self.args.coordinator_ip)
        self.run(boss, 'net', 'init', '--ip', ip)
        endpoint = self.server(boss, ip, listen)
        names = []
        for n, host in enumerate(self.args.machines, 1):
            arch = self.prepare(host)
            name = f'worker-{n}'
            names.append(name)
            home, leader = self.roots[host], self.roots[boss]
            started = time.monotonic()
            self.run(boss, 'net', 'enroll', '--machine', name, '--agent-id', 'demo', '--role', 'worker',
                     '--grant-space', 'demo', '--credential-out', leader + '/' + name + '.credential',
                     '--bundle-out', leader + '/' + name + '.bundle', '--dial', endpoint, '--server-name', ip)
            bundle = self.transfer(boss, leader + '/' + name + '.bundle', host, name + '.bundle')
            self.run(host, 'net', 'join', bundle, '--consume', dialflag)
            self.ssh(boss, 'rm -f ' + shlex.quote(leader + '/' + name + '.bundle'))
            self.run(host, 'net', 'status')
            joined = time.monotonic() - started
            self.run(host, 'seed', n, helper=True)
            worker_ip, worker_listen, worker_dial = self.ip(host)
            self.run(host, 'net', 'init', '--ip', worker_ip)
            self.run(host, 'team', 'configure', '--machine', name, '--project', 'checkout',
                     '--root', home + '/project', '--team-space', 'team.checkout', '--recipient', 'leader:lead')
            started = time.monotonic()
            self.run(host, 'team', 'enable')
            self.run(host, 'team', 'publish')
            published = time.monotonic() - started
            worker_endpoint = self.server(host, worker_ip, worker_listen)
            self.run(host, 'net', 'enroll', '--machine', 'leader', '--agent-id', 'lead', '--role', 'reader',
                     '--grant-space', 'team.checkout', '--credential-out', home + '/reader.credential')
            self.run(host, 'net', 'share', 'grant', '--machine', 'leader', '--agent-id', 'lead',
                     '--space', 'team.checkout', '--state-dir', home + '/.brain/net')
            origin = self.run(host, 'team', 'manifest')['manifests'][0]['manifest']['origin']
            ca = self.transfer(host, home + '/.brain/net/tls/ca.pem', boss, name + '.ca.pem')
            credential = self.transfer(host, home + '/reader.credential', boss, name + '.reader')
            self.run(boss, 'team', 'peer', '--machine', name, '--origin', origin, '--project', 'checkout',
                     '--team-space', 'team.checkout', '--dial', worker_endpoint, '--server-name', worker_ip,
                     '--ca', ca, '--credential', credential, worker_dial)
            self.rows.append([name, arch, joined, published, 0, host])
        for question, allowed in [('Чем заняты машины?', {'task', 'commit', 'session', 'participation'}),
                                  ('Какие ошибки сейчас открыты?', {'error'}),
                                  ('Какие решения не сработали и почему?', {'decision', 'dead_end'})]:
            self.ask(names, question, allowed)
        first = self.args.machines[0]
        self.run(first, 'team', 'disable', '--class', 'error')
        result = self.ask(names[:1], 'reopened')
        check(not result['hits'] and result['machines'][0]['status'] == 'denied', 'Старый срез не закрыт')
        self.run(first, 'team', 'publish')
        result = self.ask(names[:1], 'reopened')
        check(not result['hits'] and result['machines'][0]['status'] in ('ok', 'empty'), 'Выключенный класс виден')
        self.run(self.args.machines[-1], 'stop', helper=True)
        result = self.ask(names[-1:], 'backoff')
        check(not result['hits'] and result['machines'][0]['status'] == 'offline', 'Офлайн-машина выдала старые факты')
        print('\nПроверено: класс выключен; машина офлайн — нет ответа')
        print('Машина (SSH) | Арх. | Join, с | Publish, с | Ответов (фактов)')
        for name, arch, joined, published, count, host in self.rows:
            print(f'{name} ({host}) | {arch} | {joined:.2f} | {published:.2f} | {count}')

    def close(self):
        failures = []
        for host in reversed(self.hosts):
            root = self.roots.get(host)
            # Even a partial upload or a lost mkdir reply needs cleanup. Stop only
            # if a server PID exists; no helper is needed before server startup.
            remote = shlex.quote(root) if root else '"$HOME/' + self.name + '"'
            commands = [
                'if test -f ' + remote + '/serve.pid; then python3 ' + remote + '/stand-team-remote.py stop; fi',
                'rm -f ' + remote + '/*.bundle',
            ]
            if not self.args.keep:
                commands.append('rm -rf -- ' + remote)
            host_failed = False
            for command in commands:
                try:
                    self.stage = 'уборка'
                    self.ssh(host, command)
                except RuntimeError:
                    host_failed = True
            if host_failed:
                failures.append(host)
            elif self.args.keep:
                print(f'Сохранено (--keep): {host}:{root or self.name}; сервер остановлен')
        check(not failures, 'Уборка не завершена: ' + ', '.join(failures) + '; каталог ' + self.name)
        print('Уборка: серверы остановлены; ' + ('каталоги сохранены' if self.args.keep else 'каталоги удалены'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--coordinator', required=True)
    parser.add_argument('--machines', required=True)
    parser.add_argument('--coordinator-ip')
    target = Path(os.environ.get('CARGO_TARGET_DIR', HERE.parent / 'target'))
    parser.add_argument('--bin-x86', default=str(target / 'x86_64-unknown-linux-musl/release/brain'))
    parser.add_argument('--bin-arm', default=str(target / 'aarch64-unknown-linux-musl/release/brain'))
    parser.add_argument('--port', type=int)
    parser.add_argument('--keep', action='store_true')
    args = parser.parse_args()
    args.machines = args.machines.split(',')
    hosts = [args.coordinator, *args.machines]
    check(len(args.machines) <= 16 and len(set(hosts)) == len(hosts), 'Нужны разные SSH-алиасы, до 16 машин')
    check(all(re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*', h) for h in hosts), 'Недопустимый SSH-алиас')
    check(args.port is None or 1 <= args.port <= 65535, 'Недопустимый порт')
    os.umask(0o077)
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    with tempfile.TemporaryDirectory(prefix='brain-stand-transfer-') as local:
        stand = Stand(args, local)
        try:
            stand.execute()
        finally:
            # A second signal must not interrupt the bounded cleanup attempt.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            stand.close()


if __name__ == '__main__':
    try:
        main()
    except RuntimeError as error:
        print(f'Ошибка стенда: {error}', file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, IndexError, KeyError, KeyboardInterrupt):
        # Avoid exception payloads that can contain untrusted remote output/secrets.
        print('Стенд завершился с ошибкой. Проверьте этап и доступность SSH/бинарников.', file=sys.stderr)
        sys.exit(1)
