#!/usr/bin/env python3
"""Offline export contract tests using tiny committed repositories and fake Cargo."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('public_privacy', ROOT / 'tools/check-public-personal.py')
PRIVACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PRIVACY)


class ExportContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='public-export-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / 'source'
        self.repo.mkdir()
        self.env = dict(os.environ, GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1',
                        GIT_AUTHOR_NAME='Private Test', GIT_AUTHOR_EMAIL='private@example.invalid',
                        GIT_COMMITTER_NAME='Private Test', GIT_COMMITTER_EMAIL='private@example.invalid')
        for relative in ('tools/export-public.sh', 'tools/export-public.py', 'tools/check-public-personal.py'):
            target = self.repo / relative
            target.parent.mkdir(exist_ok=True)
            shutil.copy2(ROOT / relative, target)
        (self.repo / 'packaging').mkdir()
        (self.repo / 'packaging/public-exclude.txt').write_text('internal/\n')
        (self.repo / 'internal').mkdir()
        (self.repo / 'internal/report.md').write_text('internal evidence')
        (self.repo / 'README.md').write_text('committed public source')
        (self.repo / 'tools/check-release-secrets.py').write_text('print("clean synthetic repository")\n')
        self.git('init', '-q')
        self.git('add', '.')
        self.git('commit', '-qm', 'private history')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        cargo = self.bin / 'cargo'
        cargo.write_text('''#!/usr/bin/env python3
import os
from pathlib import Path
assert '--offline' in __import__('sys').argv
assert '--locked' in __import__('sys').argv
assert os.environ['CARGO_BUILD_JOBS'] == '4'
assert os.environ['BRAIN_OFFLINE'] == 'true'
p = Path(os.environ['CARGO_TARGET_DIR']) / 'debug/brain'
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text('#!/usr/bin/env python3\\nimport json, sys\\nprint("synthetic model warning", file=sys.stderr)\\nprint(json.dumps({"found": 1}))\\n')
p.chmod(0o755)
''')
        cargo.chmod(0o755)
        self.env.update(PATH=str(self.bin) + ':' + os.environ['PATH'],
                        CARGO_TARGET_DIR=str(self.root / 'target'))
        for name in ('PUBLIC_EXPORT_AUTHOR_NAME', 'PUBLIC_EXPORT_AUTHOR_EMAIL'):
            self.env.pop(name, None)

    def git(self, *args, repo=None):
        return subprocess.check_output(['git', *args], cwd=repo or self.repo, env=self.env, stderr=subprocess.DEVNULL).decode()

    def export(self, out):
        return subprocess.run(['bash', 'tools/export-public.sh', str(out)], cwd=self.repo,
                              env=self.env, capture_output=True, text=True)

    def test_head_only_single_commit_default_identity_and_preserve_destination(self):
        (self.repo / 'README.md').write_text('uncommitted private text')
        (self.repo / 'untracked.txt').write_text('must never export')
        out = self.root / 'public'
        result = self.export(out)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((out / 'README.md').read_text(), 'committed public source')
        self.assertFalse((out / 'internal').exists())
        self.assertFalse((out / 'untracked.txt').exists())
        self.assertEqual(self.git('rev-list', '--all', '--count', repo=out).strip(), '1')
        self.assertEqual(self.git('log', '-1', '--format=%s', repo=out).strip(), 'Brain ED 0.2.0')
        self.assertEqual(self.git('log', '-1', '--format=%ae', repo=out).strip(), 'contributors@example.invalid')
        self.assertEqual(self.git('remote', repo=out), '')
        self.assertEqual(self.export(out).returncode, 2)
        self.assertEqual((out / 'README.md').read_text(), 'committed public source')

    def test_privacy_finding_fails_and_lists_location(self):
        (self.repo / 'README.md').write_text('/' + 'home/private-person/data')
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')
        out = self.root / 'bad'
        self.assertEqual(self.export(out).returncode, 1)
        report = json.loads((self.root / 'bad.export-report/personal.log').read_text())
        self.assertEqual(report['findings'], [dict(path='README.md', line=1, rule='home-path')])
        self.assertFalse((self.root / 'target').exists())

    def test_explicit_author(self):
        self.env.update(PUBLIC_EXPORT_AUTHOR_NAME='Public Contributor', PUBLIC_EXPORT_AUTHOR_EMAIL='public@example.invalid')
        out = self.root / 'named'
        self.assertEqual(self.export(out).returncode, 0)
        self.assertEqual(self.git('log', '-1', '--format=%an <%ae>', repo=out).strip(), 'Public Contributor <public@example.invalid>')

    def test_overlay_ranges_and_fixture_boundaries(self):
        (self.repo / 'README.md').write_text('100.63.255.255 100.128.0.0\n100.64.0.1\n' + 'fd7a' + ':1234::1\n')
        rust = self.repo / 'sample.rs'
        rust.write_text('#[cfg(all(test, unix))]\nmod tests { const X: &str = "' + '/' + 'home/user/x"; }\nconst Y: &str = "' + '/' + 'home/user/y";\n')
        findings = PRIVACY.scan(self.repo)['findings']
        self.assertEqual([(f['path'], f['line'], f['rule']) for f in findings], [
            ('README.md', 2, 'overlay-ipv4'), ('README.md', 3, 'overlay-ipv6'),
            ('sample.rs', 3, 'home-path')])

    def test_archive_symlink_is_rejected(self):
        (self.repo / 'link').symlink_to('/etc/passwd')
        self.git('add', '.')
        self.git('commit', '-qm', 'link fixture')
        out = self.root / 'symlink'
        self.assertEqual(self.export(out).returncode, 2)
        self.assertFalse((out / 'link').exists())

    def test_rust_fixture_bounds_strings_comments_and_production_tail(self):
        text = '''#[cfg(test)]
mod tests {
    let raw = r###"} {"###;
    let escaped = "}\\\"{";
    /* { /* } */ } */
    let character = '}';
}
fn production() {}
'''
        spans = PRIVACY.rust_test_spans(text)
        self.assertEqual(len(spans), 1)
        self.assertEqual(text[spans[0][1]:].strip(), 'fn production() {}')


if __name__ == '__main__':
    unittest.main()
