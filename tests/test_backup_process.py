"""Linux CLI regressions: isolated synthetic HOME, no real archives/providers.

BRAIN_PROBE_BIN=/path/to/debug/brain python3 -B tests/test_backup_process.py
The preload shim injects process death at the transaction boundary, not a
Rust-only test seam. No database rows or command output are printed.
"""
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import subprocess
import tempfile
import unittest

BIN = os.environ.get("BRAIN_PROBE_BIN", str(Path(__file__).resolve().parents[1] / "target/debug/brain"))


class BackupProcess(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory(prefix="brain-backup-process-")
        cls.root = Path(cls.scratch.name)
        cls.shim = cls.root / "fault.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-o", str(cls.shim),
                        str(Path(__file__).parent / "fixtures/backup_fault.c"), "-ldl"], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    def setUp(self):
        self.home = Path(tempfile.mkdtemp(dir=self.root))
        self.brain = self.home / ".brain"
        (self.brain / "archive").mkdir(parents=True)
        self.seed(self.brain / "brain.sqlite", 2)
        self.seed(self.brain / "archive/sessions.sqlite", 20)
        (self.brain / "settings.json").write_text('{"synthetic":2}')
        self.snapshot = self.home / "brain-20260101-000000"
        self.snapshot.mkdir()
        self.seed(self.snapshot / "brain.sqlite", 1)
        self.seed(self.snapshot / "sessions.sqlite", 10)
        (self.snapshot / "settings.json").write_text('{"synthetic":1}')

    @staticmethod
    def seed(path, value):
        with closing(sqlite3.connect(path)) as c:
            c.execute("create table sample(n integer)")
            c.execute("insert into sample values (?)", (value,))
            c.commit()
        os.chmod(path, 0o600)

    def cli(self, *args, fault=None, prefix=()):
        env = {k: v for k, v in os.environ.items() if not k.startswith("BRAIN_")}
        env.update(HOME=str(self.home), PYTHONDONTWRITEBYTECODE="1")
        if fault:
            env.update(LD_PRELOAD=str(self.shim), BRAIN_TEST_BACKUP_FAULT=fault)
        return subprocess.run([*prefix, BIN, "--offline", "backup", *map(str, args)],
                              env=env, capture_output=True, timeout=30, umask=0)

    def state(self):
        values = []
        for rel in ["brain.sqlite", "archive/sessions.sqlite"]:
            path = self.brain / rel
            if not path.exists():
                values.append(None)
            else:
                with closing(sqlite3.connect(path)) as c:
                    values.append(c.execute("select sum(n) from sample").fetchone()[0])
        return tuple(values)

    def test_restore_switches_all_files_and_preserves_source(self):
        before = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in self.snapshot.iterdir()}
        r = self.cli("restore", self.snapshot, "--yes")
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.state(), (1, 10))
        self.assertEqual(json.loads((self.brain / "settings.json").read_text())["synthetic"], 1)
        self.assertEqual(before, {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in self.snapshot.iterdir()})
        self.assertTrue(Path(json.loads(r.stdout)["safety_snapshot"]).is_dir())

    def test_restore_into_missing_home_preserves_existing_cli_behavior(self):
        shutil.rmtree(self.brain)
        self.assertEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
        self.assertEqual(self.state(), (1, 10))

    def test_restore_stage_failure_preserves_whole_state(self):
        r = self.cli("restore", self.snapshot, "--yes", fault="stage-failure")
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.state(), (2, 20))
        self.assertTrue((self.snapshot / "brain.sqlite").is_file())
        self.assertEqual(json.loads((self.brain / "settings.json").read_text())["synthetic"], 2)

    def test_restore_exchange_failure_preserves_whole_state(self):
        r = self.cli("restore", self.snapshot, "--yes", fault="exchange-failure")
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.state(), (2, 20))

    def test_crash_before_commit_preserves_whole_state_without_recovery_command(self):
        r = self.cli("restore", self.snapshot, "--yes", fault="crash-before")
        self.assertEqual(r.returncode, 99)
        self.assertEqual(self.state(), (2, 20))
        self.assertEqual(json.loads((self.brain / "settings.json").read_text())["synthetic"], 2)

    def test_crash_after_commit_leaves_whole_restored_state(self):
        r = self.cli("restore", self.snapshot, "--yes", fault="crash-after")
        self.assertEqual(r.returncode, 99)
        self.assertEqual(self.state(), (1, 10))
        self.assertEqual(json.loads((self.brain / "settings.json").read_text())["synthetic"], 1)
        self.assertTrue((self.snapshot / "brain.sqlite").is_file())

    def test_open_wal_holder_refused_then_restore_clears_sidecars(self):
        c = sqlite3.connect(self.brain / "brain.sqlite")
        c.execute("pragma journal_mode=WAL")
        c.execute("pragma wal_autocheckpoint=0")
        c.execute("insert into sample values (3)")
        c.commit()
        try:
            r = self.cli("restore", self.snapshot, "--yes")
            self.assertNotEqual(r.returncode, 0)
            self.assertEqual(self.state(), (5, 20))
        finally:
            c.close()
        self.assertEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
        self.assertEqual(self.state(), (1, 10))
        self.assertFalse((self.brain / "brain.sqlite-wal").exists())
        self.assertFalse((self.brain / "brain.sqlite-shm").exists())

    def test_idle_archive_holder_refused(self):
        c = sqlite3.connect(self.brain / "archive/sessions.sqlite")
        c.execute("select count(*) from sample").fetchone()
        try:
            self.assertNotEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
            self.assertEqual(self.state(), (2, 20))
        finally:
            c.close()

    def test_prune_retains_actual_newest_despite_renamed_directory(self):
        snaps = []
        for _ in range(15):
            r = self.cli("create")
            self.assertEqual(r.returncode, 0)
            snaps.append(Path(json.loads(r.stdout)["snapshot"]))
        newest = snaps[-1].with_name("brain-19000101-000000")
        snaps[-1].rename(newest)
        self.assertEqual(self.cli("prune", "--keep", "1").returncode, 0)
        self.assertTrue(newest.is_dir())
        self.assertFalse(snaps[0].exists())

    def test_prune_does_not_count_symlinks(self):
        r = self.cli("create")
        real = Path(json.loads(r.stdout)["snapshot"])
        outside = self.home / "outside"
        outside.mkdir()
        (outside / "marker").write_text("synthetic")
        (real.parent / "brain-99990101-000000").symlink_to(outside, target_is_directory=True)
        listed = json.loads(self.cli("list").stdout)
        self.assertEqual(listed["count"], 1)
        self.assertEqual(self.cli("prune", "--keep", "0").returncode, 0)
        self.assertTrue(real.is_dir())
        self.assertTrue((outside / "marker").is_file())

    def test_prune_retains_last_usable_snapshot_with_newer_empty_directory(self):
        r = self.cli("create")
        real = Path(json.loads(r.stdout)["snapshot"])
        (real.parent / "brain-99990101-000000").mkdir()
        self.assertEqual(self.cli("prune", "--keep", "0").returncode, 0)
        self.assertTrue(real.is_dir())

    def test_restore_preserves_snapshot_chronology_for_prune(self):
        first = Path(json.loads(self.cli("create").stdout)["snapshot"])
        last = Path(json.loads(self.cli("create").stdout)["snapshot"])
        # Rename the latest snapshot so both lexical and directory iteration
        # order differ from its actual creation time.
        renamed = last.with_name("brain-19000101-000000")
        last.rename(renamed)
        self.assertEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
        # Restore also creates a newer safety snapshot: keep two.
        self.assertEqual(self.cli("prune", "--keep", "2").returncode, 0)
        self.assertTrue(renamed.is_dir())
        self.assertFalse(first.exists())

    def test_prune_retains_usable_snapshots_with_tied_creation_metadata(self):
        first = Path(json.loads(self.cli("create").stdout)["snapshot"])
        last = Path(json.loads(self.cli("create").stdout)["snapshot"])
        (first / ".created-ns").write_text("1000000000")
        (last / ".created-ns").write_text("1000000000")
        self.assertEqual(self.cli("prune", "--keep", "1").returncode, 0)
        self.assertTrue(first.is_dir())
        self.assertTrue(last.is_dir())

    def test_failed_create_never_publishes_a_partial_snapshot(self):
        (self.brain / "archive/sessions.sqlite").write_bytes(b"synthetic invalid database")
        self.assertNotEqual(self.cli("create").returncode, 0)
        self.assertEqual(json.loads(self.cli("list").stdout)["count"], 0)

    def permission_trace(self):
        self.assertIsNotNone(shutil.which("strace"), "strace is required for first-byte permissions")
        trace = self.home / "permissions.trace"
        r = self.cli("create", prefix=["strace", "-o", str(trace), "-e", "trace=mkdir,openat,chmod,fchmod"])
        self.assertEqual(r.returncode, 0)
        return trace.read_text(), Path(json.loads(r.stdout)["snapshot"])

    def test_private_directories_from_first_syscall(self):
        text, snap = self.permission_trace()
        mkdirs = [line for line in text.splitlines() if line.startswith("mkdir(") and "/backups" in line]
        self.assertTrue(mkdirs)
        self.assertTrue(all(", 0700)" in line for line in mkdirs))
        self.assertEqual(stat.S_IMODE(snap.parent.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(snap.stat().st_mode), 0o700)

    def test_private_settings_from_first_syscall(self):
        text, snap = self.permission_trace()
        settings = [line for line in text.splitlines() if line.startswith("openat(") and
                    "/backups/" in line and "settings." in line and "O_CREAT" in line]
        self.assertTrue(settings)
        self.assertTrue(all(", 0600)" in line for line in settings))
        self.assertEqual(stat.S_IMODE((snap / "settings.json").stat().st_mode), 0o600)

    def test_source_settings_symlink_refused_without_touching_external_file(self):
        outside = self.home / "outside.json"
        outside.write_text("{}")
        outside.chmod(0o644)
        (self.snapshot / "settings.json").unlink()
        (self.snapshot / "settings.json").symlink_to(outside)
        self.assertNotEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
        self.assertEqual(stat.S_IMODE(outside.stat().st_mode), 0o644)
        self.assertEqual(outside.read_text(), "{}")
        self.assertEqual(self.state(), (2, 20))

    def test_destination_settings_symlink_refused(self):
        outside = self.home / "outside.json"
        outside.write_text("{}")
        (self.brain / "settings.json").unlink()
        (self.brain / "settings.json").symlink_to(outside)
        self.assertNotEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
        self.assertEqual(outside.read_text(), "{}")
        self.assertEqual(self.state(), (2, 20))

    def test_source_and_destination_directory_symlinks_refused(self):
        alias = self.home / "brain-20270101-000000"
        alias.symlink_to(self.snapshot, target_is_directory=True)
        self.assertNotEqual(self.cli("restore", alias, "--yes").returncode, 0)
        archive = self.brain / "archive"
        outside = self.home / "outside-archive"
        archive.rename(outside)
        archive.symlink_to(outside, target_is_directory=True)
        before = hashlib.sha256((outside / "sessions.sqlite").read_bytes()).hexdigest()
        self.assertNotEqual(self.cli("restore", self.snapshot, "--yes").returncode, 0)
        self.assertEqual(before, hashlib.sha256((outside / "sessions.sqlite").read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
