#!/usr/bin/env python3
"""Implementation of export-public.sh. All source inputs come from pinned HEAD."""
import fnmatch
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile


def run(args, cwd, env=None, **kwargs):
    return subprocess.run(args, cwd=cwd, env=env, check=True, **kwargs)


def main():
    source = Path(__file__).resolve().parents[1]
    out = Path(sys.argv[1]).expanduser().resolve()
    # Refuse all pre-existing destinations, even empty ones: no overwrite/delete.
    if out.exists() or out.is_symlink():
        raise ValueError('OUT_DIR must not exist; existing exports are preserved')
    if source == out or source in out.parents:
        raise ValueError('OUT_DIR must be outside the source repository')
    source_home = Path.home()
    forbidden = source_home / '.brain'
    if out == forbidden or forbidden in out.parents or out in forbidden.parents:
        raise ValueError('OUT_DIR must not contain or be inside the Brain data directory')
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
               GIT_TERMINAL_PROMPT='0', GIT_NO_LAZY_FETCH='1')
    # Do not inherit repository overrides, signing, hooks, templates or identities.
    for key in list(env):
        if key.startswith('GIT_') and key not in ('GIT_CONFIG_NOSYSTEM', 'GIT_CONFIG_GLOBAL',
                                                 'GIT_TERMINAL_PROMPT', 'GIT_NO_LAZY_FETCH'):
            del env[key]
    git = ['git', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgSign=false',
           '-c', 'protocol.allow=never']
    head = subprocess.check_output(git + ['rev-parse', 'HEAD'], cwd=source, env=env).decode().strip()
    archive = subprocess.check_output(git + ['archive', '--format=tar', head], cwd=source, env=env)
    report_dir = out.with_name(out.name + '.export-report')
    if report_dir.exists():
        raise ValueError('report directory already exists; preserve previous evidence')
    report_dir.mkdir(parents=True)
    report = dict(source_commit=head, excluded=[], checks=[], status='incomplete')
    try:
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            policy = tar.extractfile('packaging/public-exclude.txt').read().decode()
            patterns = [s.strip() for s in policy.splitlines() if s.strip() and not s.lstrip().startswith('#')]
            out.mkdir(parents=True)
            for member in tar.getmembers():
                name = member.name.rstrip('/')
                matched = [p for p in patterns if (name == p[:-1] or name.startswith(p))
                           if p.endswith('/')]
                matched += [p for p in patterns if not p.endswith('/') and fnmatch.fnmatchcase(name, p)]
                if matched:
                    if not member.isdir():
                        report['excluded'].append(dict(path=name, patterns=matched))
                    continue
                if member.issym() or member.islnk() or not (member.isfile() or member.isdir()):
                    raise ValueError('unsupported archive entry: ' + name)
                tar.extract(member, out, filter='data')
        run(git + ['init', '--template=', '--initial-branch=main'], out, env, stdout=subprocess.DEVNULL)
        # Explicit export-only identity; never fall back to Git/user config.
        identity = dict(env,
            GIT_AUTHOR_NAME=os.environ.get('PUBLIC_EXPORT_AUTHOR_NAME', 'Brain ED Contributors'),
            GIT_AUTHOR_EMAIL=os.environ.get('PUBLIC_EXPORT_AUTHOR_EMAIL', 'contributors@example.invalid'))
        identity.update(GIT_COMMITTER_NAME=identity['GIT_AUTHOR_NAME'],
                        GIT_COMMITTER_EMAIL=identity['GIT_AUTHOR_EMAIL'])
        run(git + ['add', '--all'], out, env)
        run(git + ['commit', '--quiet', '-m', 'Brain ED 0.2.0'], out, identity)
        report['public_commit'] = subprocess.check_output(git + ['rev-parse', 'HEAD'], cwd=out, env=env).decode().strip()
        def check(name, command, check_env=env, json_output=False):
            log = report_dir / (name + '.log')
            with log.open('w') as stream:
                if json_output:
                    result = subprocess.run(command, cwd=out, env=check_env, capture_output=True, text=True)
                    stream.write(result.stdout)
                    (report_dir / (name + '.stderr.log')).write_text(result.stderr)
                else:
                    result = subprocess.run(command, cwd=out, env=check_env, stdout=stream, stderr=subprocess.STDOUT)
            report['checks'].append(dict(name=name, exit_code=result.returncode, log=log.name))
            print(f'{name}: {"PASS" if result.returncode == 0 else "FAIL"} (see {log.name})', flush=True)
            return result.returncode == 0
        clean = check('secrets', ['python3', 'tools/check-release-secrets.py', '--worktree'])
        clean = check('personal', ['python3', 'tools/check-public-personal.py']) and clean
        if not clean:
            report['status'] = 'failed'
            return 1
        target = Path(os.environ.get('CARGO_TARGET_DIR', str(source_home / 'projects/brain-ed-target/CX-R82'))).absolute()
        # Caches are read from the real toolchain/cache locations; product config,
        # all HOME writes and quick-start data live in a fresh disposable sandbox.
        with tempfile.TemporaryDirectory(prefix='brain-public-verify-') as temporary:
            sandbox = Path(temporary)
            verify = {k: v for k, v in env.items() if not k.startswith(('BRAIN_', 'CODEX_', 'CLAUDE_', 'HERMES_', 'XDG_'))}
            verify.update(HOME=str(sandbox), CARGO_HOME=os.environ.get('CARGO_HOME', str(source_home / '.cargo')),
                          RUSTUP_HOME=os.environ.get('RUSTUP_HOME', str(source_home / '.rustup')),
                          CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS='4', CARGO_NET_OFFLINE='true',
                          HF_HUB_OFFLINE='1', BRAIN_OFFLINE='true', PYTHONDONTWRITEBYTECODE='1',
                          XDG_CONFIG_HOME=str(sandbox / '.config'), XDG_DATA_HOME=str(sandbox / '.local/share'),
                          XDG_CACHE_HOME=str(sandbox / '.cache'))
            build = check('build', ['nice', '-n', '10', 'cargo', 'build', '--offline', '--locked', '--workspace'], verify)
            tests = check('cargo-test', ['nice', '-n', '10', 'cargo', 'test', '--offline', '--locked', '--workspace'], verify)
            quick = False
            if build:
                binary = target / 'debug/brain'
                commands = [ [str(binary), '--version'],
                             [str(binary), '--offline', 'init'],
                             [str(binary), '--offline', 'remember', 'publicquick quartz memory', '--kind', 'fact', '--tag', 'demo'],
                             [str(binary), '--offline', 'recall', 'publicquick quartz'] ]
                quick = True
                for i, command in enumerate(commands):
                    quick = check('quick-' + str(i), command, verify, json_output=(i == 3)) and quick
                if quick:
                    recall = json.loads((report_dir / 'quick-3.log').read_text())
                    quick = recall.get('found', 0) > 0
                    report['checks'].append(dict(name='quick-recall-found', exit_code=0 if quick else 1))
            clean = build and tests and quick
        # Tests/build must not modify exported source or leave unignored files.
        status = subprocess.check_output(git + ['status', '--porcelain'], cwd=out, env=env)
        extras = subprocess.check_output(git + ['ls-files', '--others', '-z'], cwd=out, env=env)
        if extras:
            report['unexpected_files'] = [os.fsdecode(p) for p in extras.split(b'\0') if p]
            clean = False
        if status:
            report['checks'].append(dict(name='source-unchanged', exit_code=1))
            clean = False
        report['status'] = 'passed' if clean else 'failed'
        return 0 if clean else 1
    finally:
        if (out / '.git').is_dir():
            tracked = subprocess.check_output(git + ['ls-files', '-z'], cwd=out, env=env).split(b'\0')
            report['files'] = len([p for p in tracked if p])
            report['source_bytes'] = sum((out / os.fsdecode(p)).stat().st_size for p in tracked if p)
            report['directory_bytes'] = sum(p.stat().st_size for p in out.rglob('*') if p.is_file())
        (report_dir / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print('Report: ' + str(report_dir / 'report.json'), flush=True)


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.CalledProcessError, tarfile.TarError) as error:
        print('Export failed: ' + str(error), file=sys.stderr)
        raise SystemExit(2)
