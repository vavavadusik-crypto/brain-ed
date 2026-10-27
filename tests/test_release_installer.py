"""Installer boundary tests: HTTPS is mocked; payload must never be executed."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.home = self.base / 'home with spaces'
        self.home.mkdir()
        self.bin = self.base / 'bin'
        self.bin.mkdir()
        self.payload = self.base / 'payload'
        self.payload.write_text('#!/bin/sh\necho executed > "$HOME/EXECUTED"\n')
        self.manifest = self.base / 'sums'
        self.env = dict(os.environ, HOME=str(self.home),
                        PATH=f'{self.bin}:/usr/bin:/bin',
                        PAYLOAD=str(self.payload), SUMS=str(self.manifest),
                        TRACE=str(self.base / 'trace'), MACHINE='x86_64', KERNEL='Linux')
        self.script('uname', 'case "$1" in -s) echo "$KERNEL";; -m) echo "$MACHINE";; esac')
        self.script('curl', '''printf '%s\\n' "$*" >> "$TRACE"
while [ "$#" -gt 0 ]; do
  case "$1" in --output) out="$2"; shift;; https://*) url="$1";; esac
  shift
done
case "$url" in */SHA256SUMS) cp "$SUMS" "$out";; *) cp "$PAYLOAD" "$out";; esac''')
        self.sums()

    def script(self, name, body):
        path = self.bin / name
        path.write_text('#!/bin/sh\nset -eu\n' + body + '\n')
        path.chmod(0o755)

    def sums(self, arch='x86_64', digest=None, platform='unknown-linux-musl'):
        digest = digest or hashlib.sha256(self.payload.read_bytes()).hexdigest()
        self.manifest.write_text(f'{digest}  brain-0.2.0-{arch}-{platform}\n')

    def run_installer(self, *args, ok=True):
        result = subprocess.run(['bash', str(ROOT / 'install.sh'), *args],
                                env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        self.assertFalse((self.home / 'EXECUTED').exists())
        return result

    def test_install_verified_payload_and_uninstall_preserves_data(self):
        data = self.home / '.brain' / 'keep'
        data.parent.mkdir()
        data.write_text('synthetic')
        self.run_installer()
        binary = self.home / '.local/bin/brain'
        self.assertEqual(binary.read_bytes(), self.payload.read_bytes())
        self.assertTrue(os.access(binary, os.X_OK))
        self.run_installer('--uninstall', '--dry-run')
        self.assertTrue(binary.exists())
        self.run_installer('--uninstall')
        self.assertFalse(binary.exists())
        self.assertEqual(data.read_text(), 'synthetic')

    def test_mismatch_preserves_existing_binary(self):
        binary = self.home / '.local/bin/brain'
        binary.parent.mkdir(parents=True)
        binary.write_text('old binary')
        self.sums(digest='0' * 64)
        self.run_installer(ok=False)
        self.assertEqual(binary.read_text(), 'old binary')

    def test_dry_run_no_network_or_writes(self):
        before = sorted(str(p) for p in self.base.rglob('*'))
        self.run_installer('--dry-run')
        self.assertEqual(before, sorted(str(p) for p in self.base.rglob('*')))

    def test_termux_aarch64(self):
        self.env.update(MACHINE='aarch64', PREFIX='/data/data/com.termux/files/usr')
        self.sums(arch='aarch64')
        self.run_installer()
        self.assertIn('aarch64-unknown-linux-musl', (self.base / 'trace').read_text())

    def test_macos_both_architectures_and_side_effect_free_dry_run(self):
        for machine, arch in [('arm64', 'aarch64'), ('x86_64', 'x86_64')]:
            with self.subTest(machine=machine):
                self.env.update(KERNEL='Darwin', MACHINE=machine)
                self.sums(arch=arch, platform='apple-darwin')
                before = sorted(str(p) for p in self.base.rglob('*'))
                plan = self.run_installer('--dry-run')
                self.assertEqual(before, sorted(str(p) for p in self.base.rglob('*')))
                self.assertIn(f'{arch}-apple-darwin', plan.stdout)
                self.run_installer()
                self.assertIn(f'{arch}-apple-darwin', (self.base / 'trace').read_text())
                self.assertEqual((self.home / '.local/bin/brain').read_bytes(), self.payload.read_bytes())

    def test_bad_manifest_and_options(self):
        original = self.manifest.read_text()
        for text in ('', original * 2, original.replace('brain-0.2.0', 'brain-9.9.9')):
            self.manifest.write_text(text)
            self.run_installer(ok=False)
        self.run_installer('--bogus', ok=False)
        self.run_installer('--version', ok=False)
        self.env['BRAIN_RELEASE_BASE_URL'] = 'http://insecure.invalid'
        self.run_installer('--dry-run', ok=False)


if __name__ == '__main__':
    unittest.main()
