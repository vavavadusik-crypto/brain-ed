"""Linux download bundle: packaging refusals, archive checks and the installer
in a synthetic HOME. Everything runs against the real tool, the real installer
script and a real built `brain` (BRAIN_BIN), inside temporary directories;
the real home, desktop and ~/.brain are never touched.

    BRAIN_BIN=target/release/brain python3 -B -m unittest -v tests.test_linux_bundle

When the suite runs as root (a container), the installer is run as the
unprivileged `nobody` user via setpriv, because it refuses root by design.
"""
import copy
import gzip
import hashlib
import importlib.util
import time
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

TMP_PREFIX = "bundle test "
STALE_SECONDS = 6 * 3600


def remove_tree(path):
    """Remove a test tree even where the test made entries read-only."""
    def writable_then_retry(function, failed, _info):
        try:
            os.chmod(os.path.dirname(failed), 0o700)
            os.chmod(failed, 0o700)
            function(failed)
        except OSError:
            pass

    shutil.rmtree(path, onexc=writable_then_retry) if sys.version_info >= (3, 12) else \
        shutil.rmtree(path, onerror=writable_then_retry)


def setUpModule():
    # Each run copies the ~200 MB binary several times; an interrupted run would
    # otherwise leave gigabytes in a quota-limited tmpfs. Only our own stale trees.
    base = tempfile.gettempdir()
    for name in os.listdir(base):
        path = os.path.join(base, name)
        try:
            info = os.lstat(path)
        except OSError:
            continue
        if (name.startswith(TMP_PREFIX) and os.path.isdir(path) and not os.path.islink(path)
                and info.st_uid == os.getuid() and time.time() - info.st_mtime > STALE_SECONDS):
            remove_tree(path)
TOOL = os.path.join(ROOT, "tools", "package-linux.py")
BRAIN_BIN = os.environ.get("BRAIN_BIN")
SOURCE_FILES = ["LICENSE", "THIRD_PARTY_NOTICES.md", "README.md", "docs/INSTALL.md", "docs/linux-download.md",
                "packaging/install-linux.sh", "Cargo.toml"]
AS_ROOT = os.geteuid() == 0
NOBODY = 65534


def run(cmd, env=None, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, env=env, **kw)


def read(path, mode="rb"):
    with open(path, mode) as handle:
        return handle.read()


def write(path, body, mode="w"):
    with open(path, mode) as handle:
        handle.write(body)


def snapshot(top):
    out = {}
    for base, dirs, files in os.walk(top):
        for name in dirs + files:
            path = os.path.join(base, name)
            rel = os.path.relpath(path, top)
            if os.path.islink(path):
                out[rel] = ("link", os.readlink(path))
            elif os.path.isdir(path):
                out[rel] = ("dir",)
            else:
                out[rel] = ("file", hashlib.sha256(read(path)).hexdigest())
    return out


@unittest.skipUnless(BRAIN_BIN and os.path.isfile(BRAIN_BIN), "BRAIN_BIN must point at a built brain binary")
class Bundle(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix=TMP_PREFIX)
        os.chmod(self.tmp, 0o755)
        self.addCleanup(remove_tree, self.tmp)
        self.repo = os.path.join(self.tmp, "source repo")
        for rel in SOURCE_FILES:
            os.makedirs(os.path.dirname(os.path.join(self.repo, rel)), exist_ok=True)
            shutil.copy2(os.path.join(ROOT, rel), os.path.join(self.repo, rel))
        g = ["git", "-C", self.repo, "-c", "user.email=t@example.invalid", "-c", "user.name=test"]
        run(["git", "init", "-q", self.repo], check=True)
        run(g + ["add", "-A"], check=True)
        run(g + ["commit", "-q", "-m", "synthetic source"], check=True)
        self.commit = run(["git", "-C", self.repo, "rev-parse", "HEAD"], check=True).stdout.strip()
        self.pkg_home = os.path.join(self.tmp, "packager home")
        os.makedirs(self.pkg_home)
        self.binary = os.path.join(self.tmp, "built", "brain")
        os.makedirs(os.path.dirname(self.binary))
        shutil.copy2(BRAIN_BIN, self.binary)

    def package(self, binary=None, commit=None, out="out"):
        env = dict(os.environ, HOME=self.pkg_home)
        return run([sys.executable, "-B", TOOL, "build", "--binary", binary or self.binary, "--source", self.repo,
                    "--commit", commit or self.commit, "--out", os.path.join(self.tmp, out)], env=env)

    def built(self):
        result = self.package()
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def unpack(self, archive):
        # Only an archive that `verify` accepts is unpacked, with real GNU tar.
        ok = run([sys.executable, "-B", TOOL, "verify", archive])
        self.assertEqual(ok.returncode, 0, ok.stderr)
        dest = os.path.join(self.tmp, "unpacked")
        os.makedirs(dest)
        run(["tar", "--no-same-owner", "-xzf", archive, "-C", dest], check=True)
        top = os.path.join(dest, os.listdir(dest)[0])
        for base, dirs, files in os.walk(dest):
            for name in dirs:
                os.chmod(os.path.join(base, name), 0o755)
        return top

    # ---------- packaging ----------
    def test_bundle_records_exact_clean_source_and_is_deterministic(self):
        first = self.built()
        again = self.package(out="out2")
        self.assertEqual(json.loads(again.stdout)["sha256"], first["sha256"], "same inputs, same archive bytes")
        top = self.unpack(first["archive"])
        manifest = json.loads(read(os.path.join(top, "MANIFEST.json")))
        self.assertEqual(manifest["source"]["commit"], self.commit)
        self.assertEqual(manifest["source"]["tree"], run(["git", "-C", self.repo, "rev-parse", "HEAD^{tree}"]).stdout.strip())
        self.assertIn("UNPROVEN", manifest["portability"])
        self.assertIn("not provenance", manifest["checksums"])
        names = {f["path"] for f in manifest["files"]}
        self.assertEqual(names, {"bin/brain", "install-linux.sh", "LICENSE", "THIRD_PARTY_NOTICES.md", "README.md",
                                 "docs/INSTALL.md", "docs/linux-download.md"})
        self.assertEqual(read(os.path.join(top, "LICENSE")), read(os.path.join(self.repo, "LICENSE")))
        self.assertTrue(os.access(os.path.join(top, "install-linux.sh"), os.X_OK))
        ok = run([sys.executable, "-B", TOOL, "verify", first["archive"]])
        self.assertEqual(ok.returncode, 0, ok.stderr)

    def test_dirty_untracked_or_mismatching_source_is_refused(self):
        write(os.path.join(self.repo, "README.md"), "local edit\n", "a")
        self.assertIn("not clean", self.package().stderr)
        run(["git", "-C", self.repo, "checkout", "--", "README.md"], check=True)
        write(os.path.join(self.repo, "stray.txt"), "")
        self.assertIn("not clean", self.package().stderr)
        os.remove(os.path.join(self.repo, "stray.txt"))
        self.assertIn("does not match", self.package(commit="0" * 40).stderr)
        self.assertIn("full 40-character", self.package(commit=self.commit[:12]).stderr)
        self.assertEqual(self.package().returncode, 0)

    def test_bad_binaries_are_refused(self):
        link = os.path.join(self.tmp, "link-brain")
        os.symlink(self.binary, link)
        self.assertIn("symlinks are refused", self.package(binary=link).stderr)
        script = os.path.join(self.tmp, "script")
        write(script, body="#!/bin/sh\necho hi\n")
        self.assertIn("not an ELF file", self.package(binary=script).stderr)
        for planted, why in [(b"token ghp_" + b"A" * 36, "secret"), (self.pkg_home.encode() + b"/.ssh/id", "private path")]:
            bad = os.path.join(self.tmp, "bad", "brain")
            os.makedirs(os.path.dirname(bad), exist_ok=True)
            shutil.copy2(self.binary, bad)
            with open(bad, "ab") as handle:
                handle.write(b"\0" + planted + b"\0")
            result = self.package(binary=bad)
            self.assertEqual(result.returncode, 2)
            self.assertIn(why, result.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "out")), "nothing written when refused")

    # ---------- archive checks ----------
    def rewrite(self, archive, edit, name="edited.tar.gz"):
        members = []
        with tarfile.open(archive) as tar:
            for m in tar:
                members.append((m, tar.extractfile(m).read() if m.isreg() else None))
        path = os.path.join(self.tmp, name)
        with tarfile.open(path, "w:gz") as tar:
            for m, body in edit(members):
                tar.addfile(m, io.BytesIO(body) if body is not None else None)
        return path

    def test_unsafe_or_corrupted_archives_are_refused(self):
        archive = self.built()["archive"]
        verify = lambda path, *extra: run([sys.executable, "-B", TOOL, "verify", path, *extra])

        def traversal(ms):
            evil = tarfile.TarInfo(ms[0][0].name + "/../../escape"); evil.size = 1
            return ms + [(evil, b"x")]

        def link(ms):
            evil = tarfile.TarInfo(ms[0][0].name + "/bin/other"); evil.type = tarfile.SYMTYPE; evil.linkname = "/etc/passwd"
            return ms + [(evil, None)]

        def absolute(ms):
            evil = tarfile.TarInfo("/tmp/abs"); evil.size = 1
            return ms + [(evil, b"x")]

        def corrupt(ms):
            return [(m, (b[:-1] + bytes([b[-1] ^ 1])) if m.name.endswith("LICENSE") else b) for m, b in ms]

        def extra(ms):
            evil = tarfile.TarInfo(ms[0][0].name + "/unlisted"); evil.size = 1
            return ms + [(evil, b"x")]

        for edit, why in [(traversal, "non-canonical"), (link, "not a regular"), (absolute, "non-canonical"),
                          (corrupt, "checksum mismatch"), (extra, "unexpected bundle layout")]:
            result = verify(self.rewrite(archive, edit, edit.__name__ + ".tar.gz"))
            self.assertEqual(result.returncode, 2, edit.__name__)
            self.assertIn(why, result.stderr, edit.__name__)
        self.assertIn("at most 1000 bytes", verify(archive, "--max-bytes", "1000").stderr)

    # ---------- installer in a synthetic HOME ----------
    def home(self):
        home = os.path.join(self.tmp, "home with space")
        os.makedirs(os.path.join(home, ".config"))
        os.makedirs(os.path.join(home, "Desk top"))
        write(os.path.join(home, ".config", "user-dirs.dirs"), body='XDG_DESKTOP_DIR="$HOME/Desk top"\n')
        return home

    def chown(self, *tops):
        if AS_ROOT:
            for top in tops:
                for base, dirs, files in os.walk(top):
                    for name in [base] + [os.path.join(base, n) for n in dirs + files]:
                        os.lchown(name, NOBODY, NOBODY)

    def install(self, top, home, *args):
        env = {"PATH": "/usr/bin:/bin", "HOME": home, "XDG_DATA_HOME": os.path.join(home, ".local", "share"),
               "XDG_CONFIG_HOME": os.path.join(home, ".config"), "LANG": "C.UTF-8"}
        cmd = [os.path.join(top, "install-linux.sh"), *args]
        if AS_ROOT:
            cmd = ["setpriv", f"--reuid={NOBODY}", f"--regid={NOBODY}", "--clear-groups", *cmd]
        return run(cmd, env=env, cwd="/")

    def test_install_dry_run_uninstall_keeps_data(self):
        top = self.unpack(self.built()["archive"])
        home = self.home()
        self.chown(top, home)
        self.assertIn("Usage:", self.install(top, home, "--help").stdout)
        before = snapshot(home)
        dry = self.install(top, home, "--dry-run")
        self.assertEqual(dry.returncode, 0, dry.stderr)
        self.assertEqual(snapshot(home), before, "dry run writes nothing")
        done = self.install(top, home)
        self.assertEqual(done.returncode, 0, done.stderr)
        stable = os.path.join(home, ".brain", "bin", "brain")
        self.assertTrue(os.access(stable, os.X_OK))
        apps = os.path.join(home, ".local", "share", "applications")
        for name in ("brain-for-ai-settings.desktop", "brain-for-ai-graph.desktop"):
            for where in (apps, os.path.join(home, "Desk top")):
                entry = read(os.path.join(where, name), "r")
                self.assertIn('Exec="' + stable + '" ui --port', entry, "path with a space is one quoted argument")
        # User data written after install must survive uninstall.
        data = {os.path.join(home, ".brain", "brain.sqlite"): b"synthetic db",
                os.path.join(home, ".brain", "settings.json"): b'{"synthetic": true}'}
        for path, body in data.items():
            write(path, body, "wb")
        self.chown(home)
        before = snapshot(home)
        dry = self.install(top, home, "--uninstall", "--dry-run")
        self.assertEqual(dry.returncode, 0, dry.stderr)
        self.assertEqual(snapshot(home), before, "uninstall dry run writes nothing")
        gone = self.install(top, home, "--uninstall")
        self.assertEqual(gone.returncode, 0, gone.stderr)
        for name in ("brain-for-ai-settings.desktop", "brain-for-ai-graph.desktop"):
            self.assertFalse(os.path.exists(os.path.join(apps, name)))
            self.assertFalse(os.path.exists(os.path.join(home, "Desk top", name)))
        for path, body in data.items():
            self.assertEqual(read(path), body, "data under ~/.brain is kept")

    def test_installer_refuses_bad_bundles_before_running_the_binary(self):
        archive = self.built()["archive"]
        cases = {
            "corrupted": lambda t: write(os.path.join(t, "docs", "INSTALL.md"), mode="a", body="x"),
            "extra file": lambda t: write(os.path.join(t, "extra.sh"), body="x"),
            "symlink": lambda t: os.symlink("/etc/passwd", os.path.join(t, "docs", "link")),
            "traversal": lambda t: write(os.path.join(t, "SHA256SUMS"), mode="a", body="0" * 64 + "  ../outside\n"),
            "oversized sums": lambda t: write(os.path.join(t, "SHA256SUMS"), mode="a", body=("0" * 64 + "  x\n") * 70),
            "missing sums": lambda t: os.remove(os.path.join(t, "SHA256SUMS")),
            "duplicate row": lambda t: write(os.path.join(t, "SHA256SUMS"), read(os.path.join(t, "SHA256SUMS"), "r").splitlines(True)[0], "a"),
            "unreadable dir": lambda t: os.chmod(os.path.join(t, "docs"), 0),
        }
        for label, damage in cases.items():
            with self.subTest(label):
                shutil.rmtree(os.path.join(self.tmp, "unpacked"), True)
                top = self.unpack(archive)
                damage(top)
                home = self.home() if not os.path.exists(os.path.join(self.tmp, "home with space")) else os.path.join(self.tmp, "home with space")
                self.chown(top, home)
                before = snapshot(home)
                result = self.install(top, home)
                self.assertNotEqual(result.returncode, 0, label)
                self.assertIn("install-linux:", result.stderr)
                self.assertEqual(snapshot(home), before, label + ": nothing written, binary never ran")
                undo = self.install(top, home, "--uninstall")
                self.assertNotEqual(undo.returncode, 0, label + ": uninstall also verifies the bundle first")
                os.chmod(os.path.join(top, "docs"), 0o755)

    # ---------- regressions: D1 duplicate/alias members, PAX, bombs, manifest ----------
    def members_of(self, archive):
        out = []
        with tarfile.open(archive) as tar:
            for m in tar:
                out.append((m, tar.extractfile(m).read() if m.isreg() else None))
        return out

    def pack(self, members, name, fmt=tarfile.GNU_FORMAT):
        path = os.path.join(self.tmp, name)
        with tarfile.open(path, "w:gz", format=fmt) as tar:
            for m, body in members:
                m = copy.copy(m)
                if body is not None:
                    m.size = len(body)
                tar.addfile(m, io.BytesIO(body) if body is not None else None)
        return path

    def resum(self, members):
        """Rewrite SHA256SUMS so it matches the edited files: only the check
        under test can then catch the edit."""
        top = members[0][0].name
        bodies = {m.name[len(top) + 1:]: b for m, b in members if b is not None}
        sums = "".join(f"{hashlib.sha256(b).hexdigest()}  {n}\n" for n, b in sorted(bodies.items()) if n != "SHA256SUMS").encode()
        return [(m, sums if m.name.endswith("/SHA256SUMS") else b) for m, b in members]

    def verify(self, path, *extra):
        result = run([sys.executable, "-B", TOOL, "verify", path, *extra])
        self.assertNotIn("Traceback", result.stderr)
        return result

    def test_d1_duplicate_alias_and_layout_members_are_refused(self):
        archive = self.built()["archive"]
        ms = self.members_of(archive)
        top = ms[0][0].name
        lic = next((m, b) for m, b in ms if m.name.endswith("/LICENSE"))
        bad = copy.copy(lic[0])
        cases = {
            # D1: an unchecked bad LICENSE followed by the correct one must not pass.
            "bad then good duplicate": ([(bad, b"tampered licence\n")] + ms, "duplicate member"),
            "dot alias": (ms + [(self.ti(top + "/./LICENSE"), b"x")], "non-canonical"),
            "empty component": (ms + [(self.ti(top + "/docs//INSTALL.md"), b"x")], "non-canonical"),
            "backslash": (ms + [(self.ti(top + "/docs\\x"), b"x")], "non-canonical"),
            "file under a file": (ms + [(self.ti(top + "/LICENSE/x"), b"x")], "collides"),
            "dir and file same name": (ms + [(self.ti(top + "/LICENSE", tarfile.DIRTYPE), None)], "duplicate member"),
            "missing required file": ([x for x in ms if not x[0].name.endswith("/README.md")], "unexpected bundle layout"),
        }
        for label, (members, why) in cases.items():
            with self.subTest(label):
                result = self.verify(self.pack(members, label.replace(" ", "_") + ".tar.gz"))
                self.assertEqual(result.returncode, 2, label)
                self.assertIn(why, result.stderr, label)

    def ti(self, name, kind=tarfile.REGTYPE):
        info = tarfile.TarInfo(name)
        info.type = kind
        return info

    def test_extension_headers_bombs_and_sums_bounds_are_refused(self):
        archive = self.built()["archive"]
        ms = self.members_of(archive)
        top = ms[0][0].name
        pax = [(copy.copy(m), b) for m, b in ms]
        pax[-1][0].pax_headers = {"comment": "x"}
        self.assertIn("extension header", self.verify(self.pack(pax, "pax.tar.gz", tarfile.PAX_FORMAT)).stderr)
        longpath = top + "/docs/" + "/".join(["abcdefghij"] * 12)
        self.assertIn("extension header", self.verify(self.pack(ms + [(self.ti(longpath), b"x")], "long.tar.gz")).stderr)
        bomb = os.path.join(self.tmp, "bomb.tar.gz")
        with gzip.open(bomb, "wb") as gz:
            for _ in range(40):
                gz.write(b"\0" * (1 << 20))
        result = self.verify(bomb, "--max-bytes", str(2 << 20))
        self.assertIn("decompresses to more than", result.stderr)
        sums = lambda f: [(m, f(b) if m.name.endswith("/SHA256SUMS") else b) for m, b in ms]
        self.assertIn("newline-terminated lines", self.verify(self.pack(sums(lambda b: b + b"x\n" * 70), "big.tar.gz")).stderr)
        self.assertIn("duplicate SHA256SUMS row", self.verify(self.pack(sums(lambda b: b + b.splitlines(True)[0]), "dup.tar.gz")).stderr)

    def test_manifest_is_parsed_strictly(self):
        ms = self.members_of(self.built()["archive"])
        edit = lambda f: self.resum([(m, f(b) if m.name.endswith("/MANIFEST.json") else b) for m, b in ms])
        dup = edit(lambda b: b.replace(b'"format": 1', b'"format": 1,\n "format": 1', 1))
        self.assertIn("duplicate keys", self.verify(self.pack(dup, "mdup.tar.gz")).stderr)
        wrong = edit(lambda b: b.replace(b'"size": ', b'"size": 1', 1))
        self.assertIn("do not match the bundle", self.verify(self.pack(wrong, "mwrong.tar.gz")).stderr)
        self.assertIn("not valid JSON", self.verify(self.pack(edit(lambda b: b[:-3]), "mtrunc.tar.gz")).stderr)

    # ---------- regressions: O1 output handling, S1 repository, ELF, inputs ----------
    def test_o1_existing_outputs_and_unsafe_out_dirs_are_never_written(self):
        probe = self.package(out="probe-out")  # the real archive name for this source version
        self.assertEqual(probe.returncode, 0, probe.stderr)
        archive_name = os.path.basename(json.loads(probe.stdout)["archive"])
        out = os.path.join(self.tmp, "out")
        os.mkdir(out, 0o755)
        sentinel = os.path.join(self.tmp, "sentinel")
        write(sentinel, "synthetic sentinel\n")
        name = os.path.join(out, archive_name)
        os.symlink(sentinel, name)
        result = self.package()
        self.assertEqual(result.returncode, 2)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(read(sentinel, "r"), "synthetic sentinel\n")
        self.assertTrue(os.path.islink(name))
        os.remove(name)
        write(name, "existing archive")
        self.assertIn("already exists", self.package().stderr)
        self.assertEqual(read(name, "r"), "existing archive")
        self.assertEqual(sorted(os.listdir(out)), [os.path.basename(name)], "no staging leftovers")
        os.symlink(out, os.path.join(self.tmp, "outlink"))
        self.assertIn("symlinks are refused", self.package(out="outlink").stderr)
        os.mkdir(os.path.join(self.tmp, "shared"), 0o755)
        os.chmod(os.path.join(self.tmp, "shared"), 0o777)
        self.assertIn("not group/world-writable", self.package(out="shared").stderr)
        inside = run([sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", self.repo,
                      "--commit", self.commit, "--out", os.path.join(self.repo, "dist")], env=dict(os.environ, HOME=self.pkg_home))
        self.assertIn("outside the source tree", inside.stderr)

    def test_s1_repository_is_strict_and_generated_metadata_is_scanned(self):
        token = "ghp_" + "A" * 36
        env = dict(os.environ, HOME=self.pkg_home)
        base = [sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", self.repo, "--commit", self.commit,
                "--out", os.path.join(self.tmp, "out")]
        for url in [f"https://example.invalid/repo?token={token}", "https://user:pass@example.invalid/repo",
                    "https://example.invalid/repo#frag", "http://example.invalid/repo", "file:///etc/passwd"]:
            result = run(base + ["--repository", url], env=env)
            self.assertEqual(result.returncode, 2, url)
            self.assertIn("--repository must be", result.stderr, url)
        result = run(base + ["--repository", f"https://example.invalid/{token}"], env=env)
        self.assertEqual(result.returncode, 2)
        self.assertIn("MANIFEST.json contains something that looks like a secret", result.stderr)
        self.assertFalse(os.path.lexists(os.path.join(self.tmp, "out")), "refused before any write: no output directory")

    def test_malformed_elf_fifo_and_unsafe_sources_are_refused_cleanly(self):
        good = read(self.binary)
        def variant(label, data):
            path = os.path.join(self.tmp, "elf", label)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            write(path, data, "wb")
            return path
        phnum_huge = bytearray(good[:4096]); phnum_huge[56:58] = (128).to_bytes(2, "little")
        rel = bytearray(good); rel[16:18] = (1).to_bytes(2, "little")
        bad_phoff = bytearray(good[:4096]); bad_phoff[32:40] = (10 ** 9).to_bytes(8, "little")
        for label, data, why in [("truncated", good[:63], "not an ELF file"), ("phnum", bytes(phnum_huge), "malformed ELF"),
                                 ("et_rel", bytes(rel), "ET_EXEC/ET_DYN"), ("phoff", bytes(bad_phoff), "malformed ELF")]:
            result = self.package(binary=variant(label, data))
            self.assertEqual(result.returncode, 2, label)
            self.assertIn(why, result.stderr, label)
            self.assertNotIn("Traceback", result.stderr, label)
        fifo = os.path.join(self.tmp, "fifo")
        os.mkfifo(fifo)
        result = run([sys.executable, "-B", TOOL, "build", "--binary", fifo, "--source", self.repo, "--commit", self.commit,
                      "--out", os.path.join(self.tmp, "out")], env=dict(os.environ, HOME=self.pkg_home), timeout=30)
        self.assertIn("not a regular file", result.stderr)
        sub = run([sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", os.path.join(self.repo, "docs"),
                   "--commit", self.commit, "--out", os.path.join(self.tmp, "out")], env=dict(os.environ, HOME=self.pkg_home))
        self.assertIn("top of a Git work tree", sub.stderr)
        os.symlink(self.repo, os.path.join(self.tmp, "repolink"))
        link = run([sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", os.path.join(self.tmp, "repolink"),
                    "--commit", self.commit, "--out", os.path.join(self.tmp, "out")], env=dict(os.environ, HOME=self.pkg_home))
        self.assertIn("symlinks are refused", link.stderr)

    def test_source_git_config_that_runs_programs_is_refused_before_git_runs(self):
        sentinel = os.path.join(self.tmp, "hook-ran")
        for key, value in [("filter.evil.clean", f"touch '{sentinel}'"), ("core.fsmonitor", f"touch '{sentinel}'"),
                           ("core.hooksPath", self.tmp), ("include.path", "/dev/null")]:
            with self.subTest(key):
                run(["git", "-C", self.repo, "config", key, value], check=True)
                write(os.path.join(self.repo, ".gitattributes"), "* filter=evil\n") if key.startswith("filter") else None
                result = self.package()
                self.assertEqual(result.returncode, 2, key)
                self.assertIn("can run programs", result.stderr, key)
                self.assertFalse(os.path.exists(sentinel), key + ": no program ran")
                run(["git", "-C", self.repo, "config", "--unset", key], check=True)
                if os.path.exists(os.path.join(self.repo, ".gitattributes")):
                    os.remove(os.path.join(self.repo, ".gitattributes"))
        self.assertEqual(self.package().returncode, 0)

    def test_unsafe_modes_top_name_and_manifest_schema_are_refused(self):
        ms = self.members_of(self.built()["archive"])
        top = ms[0][0].name
        def with_mode(suffix, mode):
            out = []
            for m, b in ms:
                m = copy.copy(m)
                if m.name.endswith(suffix):
                    m.mode = mode
                out.append((m, b))
            return out
        self.assertIn("unsafe mode", self.verify(self.pack(with_mode("/bin/brain", 0o4755), "suid.tar.gz")).stderr)
        self.assertIn("unsafe mode", self.verify(self.pack(with_mode("/LICENSE", 0o666), "ww.tar.gz")).stderr)
        renamed = []
        for m, b in ms:
            m = copy.copy(m)
            m.name = "brain-ed-9.9.9-linux-x86_64" + m.name[len(top):]
            renamed.append((m, b))
        self.assertIn("unexpected top directory", self.verify(self.pack(renamed, "top.tar.gz")).stderr)
        extra = self.resum([(m, b.replace(b'"format": 1', b'"extra": 1,\n "format": 1', 1) if m.name.endswith("/MANIFEST.json") else b) for m, b in ms])
        self.assertIn("exactly the keys", self.verify(self.pack(extra, "mkeys.tar.gz")).stderr)

    def test_literal_home_path_is_private_too(self):
        link_home = os.path.join(self.tmp, "home-link")
        os.symlink(self.pkg_home, link_home)
        bad = os.path.join(self.tmp, "bad2", "brain")
        os.makedirs(os.path.dirname(bad))
        shutil.copy2(self.binary, bad)
        write(bad, b"\0" + link_home.encode() + b"/x\0", "ab")
        result = run([sys.executable, "-B", TOOL, "build", "--binary", bad, "--source", self.repo, "--commit", self.commit,
                      "--out", os.path.join(self.tmp, "out")], env=dict(os.environ, HOME=link_home))
        self.assertIn("private path", result.stderr)

    def edit_manifest(self, ms, change):
        def apply(body):
            data = json.loads(body)
            change(data)
            return (json.dumps(data, indent=1, sort_keys=True) + "\n").encode()
        return self.resum([(m, apply(b) if m.name.endswith("/MANIFEST.json") else b) for m, b in ms])

    def test_manifest_field_types_and_target_agreement(self):
        ms = self.members_of(self.built()["archive"])
        probes = {
            "path is a list": (lambda m: m["files"][0].__setitem__("path", []), "files path"),
            "format true": (lambda m: m.__setitem__("format", True), "format"),
            "target extra key": (lambda m: m["target"].__setitem__("unexpected", True), "target must be an object"),
            "target linkage differs from ELF": (lambda m: m["target"].__setitem__("linkage", "static"), "does not match the bundled ELF"),
            "size true": (lambda m: m["files"][0].__setitem__("size", True), "files size"),
            "commit_time string": (lambda m: m["source"].__setitem__("commit_time", "1"), "commit_time"),
            "sha uppercase": (lambda m: m["files"][0].__setitem__("sha256", m["files"][0]["sha256"].upper()), "files sha256"),
            "source extra key": (lambda m: m["source"].__setitem__("x", 1), "source must be an object"),
            "repository with query": (lambda m: m["source"].__setitem__("repository", "https://example.invalid/r?x=1"), "repository"),
            "files not a list": (lambda m: m.__setitem__("files", {}), "files must be a list"),
        }
        for label, (change, why) in probes.items():
            with self.subTest(label):
                result = self.verify(self.pack(self.edit_manifest(ms, change), label.replace(" ", "_") + ".tar.gz"))
                self.assertEqual(result.returncode, 2, label + ": " + result.stderr)
                self.assertIn(why, result.stderr, label)

    def test_nonfinite_and_nested_json_are_refused_without_traceback(self):
        ms = self.members_of(self.built()["archive"])
        raw = lambda f: self.resum([(m, f(b) if m.name.endswith("/MANIFEST.json") else b) for m, b in ms])
        for label, value, why in [("NaN", b"NaN", "non-finite"), ("Infinity", b"-Infinity", "non-finite"),
                                  ("overflow", b"1e400", "non-finite"), ("huge int", b"9" * 5000, "not valid JSON"),
                                  ("deep", b"[" * 20 + b"]" * 20, "too deeply nested"),
                                  ("very deep", b"[" * 20000 + b"]" * 20000, "too deeply nested")]:
            with self.subTest(label):
                result = self.verify(self.pack(raw(lambda b: b.replace(b'"format": 1', b'"format": ' + value, 1)), label + ".tar.gz"))
                self.assertEqual(result.returncode, 2, label)
                self.assertIn(why, result.stderr, label)

    def test_member_admission_is_bounded_before_bodies_are_read(self):
        ms = self.members_of(self.built()["archive"])
        path = os.path.join(self.tmp, "many.tar.gz")
        with tarfile.open(path, "w:gz", format=tarfile.GNU_FORMAT) as tar:
            for m, body in ms:
                tar.addfile(m, io.BytesIO(body) if body is not None else None)
            for i in range(50000):
                info = tarfile.TarInfo(f"{ms[0][0].name}/docs/n{i}")
                tar.addfile(info, io.BytesIO(b""))
        started = time.monotonic()
        result = self.verify(path)
        self.assertEqual(result.returncode, 2)
        self.assertIn("unexpected bundle layout", result.stderr)
        # The early per-member admission message, not the final layout check.
        self.assertIn("docs/n0' is not part of a bundle", result.stderr, "refused at the first unknown member, before later bodies")
        self.assertLess(time.monotonic() - started, 20)

    def test_g1_worktree_config_filter_never_runs_on_a_same_size_change(self):
        """Old code ran this filter during `git status`: the change keeps the
        file size, so git must compare content and would call the clean filter."""
        marker = os.path.join(self.tmp, "synthetic-marker")
        g = ["git", "-C", self.repo, "-c", "user.email=t@example.invalid", "-c", "user.name=test"]
        write(os.path.join(self.repo, ".gitattributes"), "README.md filter=evil\n")
        run(g + ["add", ".gitattributes"], check=True)
        run(g + ["commit", "-q", "-m", "attribute"], check=True)
        commit = run(["git", "-C", self.repo, "rev-parse", "HEAD"], check=True).stdout.strip()
        run(["git", "-C", self.repo, "config", "core.repositoryformatversion", "1"], check=True)
        run(["git", "-C", self.repo, "config", "extensions.worktreeConfig", "true"], check=True)
        run(["git", "-C", self.repo, "config", "--worktree", "filter.evil.clean", f"touch '{marker}'; cat"], check=True)
        readme = os.path.join(self.repo, "README.md")
        body = read(readme)
        write(readme, (b"X" if body[:1] != b"X" else b"Y") + body[1:], "wb")
        self.assertEqual(len(read(readme)), len(body), "same-length change forces a content comparison")
        result = self.package(commit=commit)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("refused", result.stderr)
        self.assertFalse(os.path.exists(marker), "the worktree-config filter never ran")
        write(readme, body, "wb")
        self.assertIn("config.worktree", self.package(commit=commit).stderr, "a clean tree with a worktree config is refused too")
        self.assertFalse(os.path.exists(marker))

    def test_submodules_extensions_and_format_versions_are_refused(self):
        dotgit = os.path.join(self.repo, ".git")
        cfg = os.path.join(dotgit, "config")
        original = read(cfg)
        append = lambda text: (lambda: write(cfg, original + text.encode(), "wb"))
        restore = lambda: write(cfg, original, "wb")
        cases = [
            ("gitmodules", lambda: write(os.path.join(self.repo, ".gitmodules"), "[submodule \"x\"]\n\tpath = x\n"),
             lambda: os.remove(os.path.join(self.repo, ".gitmodules")), ".gitmodules exists"),
            ("modules dir", lambda: os.mkdir(os.path.join(dotgit, "modules")), lambda: os.rmdir(os.path.join(dotgit, "modules")), "modules exists"),
            ("modules symlink", lambda: os.symlink(self.tmp, os.path.join(dotgit, "modules")), lambda: os.remove(os.path.join(dotgit, "modules")), "modules exists"),
            ("worktree config symlink", lambda: os.symlink("/dev/null", os.path.join(dotgit, "config.worktree")),
             lambda: os.remove(os.path.join(dotgit, "config.worktree")), "config.worktree exists"),
            # Raw config text: git itself refuses to edit some of these states.
            ("format 1", append("[core]\n\trepositoryformatversion = 1\n"), restore, "repositoryformatversion must be"),
            ("extensions section", append("[extensions]\n\tobjectFormat = sha1\n"), restore, "extensions or submodules"),
            ("submodule section", append('[submodule "x"]\n\turl = https://example.invalid/x\n'), restore, "extensions or submodules"),
        ]
        for label, make, undo, why in cases:
            with self.subTest(label):
                make()
                result = self.package()
                self.assertEqual(result.returncode, 2, label)
                self.assertIn(why, result.stderr, label)
                undo()
        self.assertEqual(self.package().returncode, 0, "a plain clone is accepted again")

    def test_e1_pt_interp_must_be_single_nonempty_and_terminated(self):
        good = bytearray(read(self.binary))
        phoff, phnum = int.from_bytes(good[32:40], "little"), int.from_bytes(good[56:58], "little")
        headers = [phoff + i * 56 for i in range(phnum)]
        kind = lambda h: int.from_bytes(good[h:h + 4], "little")
        interp = next(h for h in headers if kind(h) == 3)
        other = next(h for h in headers if kind(h) not in (1, 2, 3, 6))  # e.g. PT_NOTE / PT_GNU_STACK
        off, size = int.from_bytes(good[interp + 8:interp + 16], "little"), int.from_bytes(good[interp + 32:interp + 40], "little")
        def variant(label, edit):
            data = bytearray(good)
            edit(data)
            path = os.path.join(self.tmp, "interp", label)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            write(path, bytes(data), "wb")
            return path
        set_size = lambda n: (lambda d: d.__setitem__(slice(interp + 32, interp + 40), n.to_bytes(8, "little")))
        cases = {
            "zero size": set_size(0),
            "only NUL": lambda d: d.__setitem__(slice(off, off + size), b"\0" * size),
            "no terminator": set_size(size - 1),
            "embedded NUL": lambda d: d.__setitem__(off + 3, 0),
            "duplicate": lambda d: d.__setitem__(slice(other, other + 56), d[interp:interp + 56]),
        }
        for label, edit in cases.items():
            with self.subTest(label):
                result = self.package(binary=variant(label.replace(" ", "_"), edit))
                self.assertEqual(result.returncode, 2, label)
                self.assertIn("PT_INTERP", result.stderr, label)
                self.assertNotIn("Traceback", result.stderr, label)
        self.assertEqual(json.loads(self.package().stdout)["target"]["linkage"], "dynamic")

    def test_raw_tar_types_global_header_and_declared_sizes(self):
        ms = self.members_of(self.built()["archive"])
        def edited(suffix, change):
            out = []
            for m, b in ms:
                m = copy.copy(m)
                if m.name.endswith(suffix):
                    b = change(m, b)
                out.append((m, b))
            return out
        for kind, label in [(tarfile.CONTTYPE, "contiguous 7"), (tarfile.GNUTYPE_SPARSE, "sparse S")]:
            with self.subTest(label):
                def retype(m, b, kind=kind):
                    m.type = kind
                    return b
                result = self.verify(self.pack(edited("/LICENSE", retype), label.replace(" ", "_") + ".tar.gz"))
                self.assertEqual(result.returncode, 2, label)
                self.assertIn("not a regular file or directory", result.stderr, label)
        glob = tarfile.TarInfo("pax_global_header")
        glob.type = tarfile.XGLTYPE
        result = self.verify(self.pack([(glob, b"")] + ms, "global.tar.gz"))
        self.assertEqual(result.returncode, 2)
        self.assertIn("extension or global header", result.stderr)
        for suffix, size in [("/LICENSE", 4 * 1024 * 1024 + 1), ("/MANIFEST.json", 64 * 1024 + 1), ("/SHA256SUMS", 16 * 1024 + 1)]:
            with self.subTest(suffix):
                result = self.verify(self.pack(edited(suffix, lambda m, b, size=size: b"x" * size), suffix.strip("/") + "_big.tar.gz"))
                self.assertEqual(result.returncode, 2, suffix)
                self.assertIn("declares", result.stderr, suffix)

    def test_repositoryformatversion_is_read_conservatively(self):
        cfg = os.path.join(self.repo, ".git", "config")
        original = read(cfg)
        self.assertRegex(original.decode(), r"(?m)^\s*repositoryformatversion = 0$", "fixture is an ordinary git init config")
        forms = {
            "inline hash comment": "[core]\n repositoryformatversion = 1 # synthetic inline comment\n",
            "inline semicolon comment": "[core]\n\trepositoryformatversion = 1 ; synthetic\n",
            "quoted 1": '[core]\n\trepositoryformatversion = "1"\n',
            "quoted 0": '[core]\n\trepositoryformatversion = "0"\n',
            "bare key (true)": "[core]\n\trepositoryformatversion\n",
            "continuation": "[core]\n\trepositoryformatversion = 0\\\n1\n",
            "no spaces 1": "[core]\nrepositoryformatversion=1\n",
            "upper-case key": "[core]\n\tRepositoryFormatVersion = 1\n",
            "comment line mentioning it": "# repositoryformatversion = 1\n",
            "zero then comment": "[core]\n\trepositoryformatversion = 0 # ok?\n",
        }
        for label, text in forms.items():
            with self.subTest(label):
                write(cfg, original + text.encode(), "wb")
                result = self.package()
                self.assertEqual(result.returncode, 2, label + ": " + result.stderr)
                self.assertIn("repositoryformatversion must be exactly", result.stderr, label)
        write(cfg, original, "wb")
        self.assertEqual(self.package().returncode, 0, "the ordinary `repositoryformatversion = 0` clone is accepted")

    # ---------- C1: .git/config admission before any git command ----------
    def c1_fixture(self):
        """Committed attribute + same-length README change: git status must
        compare content and would call a configured clean filter."""
        marker = os.path.join(self.tmp, "synthetic-marker")
        g = ["git", "-C", self.repo, "-c", "user.email=t@example.invalid", "-c", "user.name=test"]
        write(os.path.join(self.repo, ".gitattributes"), "README.md filter=evil\n")
        run(g + ["add", ".gitattributes"], check=True)
        run(g + ["commit", "-q", "-m", "attribute"], check=True)
        commit = run(["git", "-C", self.repo, "rev-parse", "HEAD"], check=True).stdout.strip()
        readme = os.path.join(self.repo, "README.md")
        body = read(readme)
        write(readme, (b"X" if body[:1] != b"X" else b"Y") + body[1:], "wb")
        self.assertEqual(len(read(readme)), len(body))
        filt = b'[filter "evil"]\n\tclean = touch \'' + marker.encode() + b"'; cat\n"
        return marker, commit, filt

    def test_c1_bom_adjacent_and_cr_headers_never_run_a_filter(self):
        cfg = os.path.join(self.repo, ".git", "config")
        original = read(cfg)
        marker, commit, filt = self.c1_fixture()
        variants = {
            "UTF-8 BOM before a first [filter] section": b"\xef\xbb\xbf" + filt + original,
            "adjacent [core][filter] headers": original + b"[core]" + filt,
            "carriage return between headers": original + b"[core]\r" + filt,
        }
        for label, text in variants.items():
            with self.subTest(label):
                write(cfg, text, "wb")
                result = self.package(commit=commit)
                # Checked first: the actual proof of zero execution.
                self.assertFalse(os.path.exists(marker), label + ": the filter never ran")
                self.assertEqual(result.returncode, 2, label)
                self.assertIn("plain-clone syntax", result.stderr, label)
                self.assertNotIn("not clean", result.stderr, label + ": refused before git status ran")

    def test_config_lexical_and_allowlist_refusals_and_plain_clone_accepted(self):
        cfg = os.path.join(self.repo, ".git", "config")
        original = read(cfg)
        cases = {
            "key before any header": (b"\tbare = false\n" + original, "plain-clone syntax"),
            "indented header": (original + b" [core]\n", "plain-clone syntax"),
            "text after header": (original + b"[core] bare = false\n", "plain-clone syntax"),
            "quoted value": (original + b'[core]\n\tbare = "false"\n', "plain-clone syntax"),
            "escaped value": (original + b"[core]\n\tbare = fal\\se\n", "plain-clone syntax"),
            "continuation": (original + b"[core]\n\tbare = false\\\n", "plain-clone syntax"),
            "inline comment": (original + b"[core]\n\tbare = false # c\n", "plain-clone syntax"),
            "NUL byte": (original + b"[core]\n\tbare = fa\0lse\n", "plain-clone syntax"),
            "no final newline": (original + b"[core]\n\tbare = false", "plain-clone syntax"),
            "unknown core key": (original + b"[core]\n\tautocrlf = true\n", "which a plain clone does not"),
            "unknown section": (original + b'[user]\n\tname = x\n', "which a plain clone does not"),
            "http key": (original + b'[http "https://github.com/"]\n\textraheader = x\n', "which a plain clone does not"),
            "remote without subsection": (original + b"[remote]\n\turl = x\n", "which a plain clone does not"),
        }
        for label, (text, why) in cases.items():
            with self.subTest(label):
                write(cfg, text, "wb")
                result = self.package()
                self.assertEqual(result.returncode, 2, label)
                self.assertIn(why, result.stderr, label)
        write(cfg, original + b'[gc]\n\tauto = 0\n[http "https://github.com/"]\n', "wb")
        self.assertEqual(self.package().returncode, 0, "plain config plus gc.auto and an empty http section is accepted")
        write(cfg, original, "wb")
        clone = os.path.join(self.tmp, "plain clone")
        run(["git", "clone", "-q", self.repo, clone], check=True)
        run(["git", "-C", clone, "checkout", "-q", "-b", "feature"], check=True)
        result = run([sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", clone, "--commit", self.commit,
                      "--out", os.path.join(self.tmp, "clone-out")], env=dict(os.environ, HOME=self.pkg_home))
        self.assertEqual(result.returncode, 0, "an ordinary git clone + checkout is accepted: " + result.stderr)

    def test_k1_sparse_checkout_disable_residue_is_refused_and_a_plain_clone_is_accepted(self):
        """What actions/checkout does without sparse inputs: `git sparse-checkout
        disable`, then unset extensions.worktreeConfig. The residue must be
        refused before anything is written; an ordinary clone of the same commit
        (the workflow's own step) must package."""
        if run(["git", "-C", self.repo, "sparse-checkout", "-h"]).returncode not in (0, 129):
            self.skipTest("this git has no sparse-checkout command")
        run(["git", "-C", self.repo, "sparse-checkout", "disable"], check=True)
        run(["git", "-C", self.repo, "config", "--local", "--unset-all", "extensions.worktreeConfig"])
        residue = os.path.join(self.repo, ".git", "config.worktree")
        self.assertTrue(os.path.lexists(residue), "precondition: this git leaves .git/config.worktree behind")
        out = os.path.join(self.tmp, "k1-out")
        refused = run([sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", self.repo,
                       "--commit", self.commit, "--out", out], env=dict(os.environ, HOME=self.pkg_home))
        self.assertEqual(refused.returncode, 2, refused.stderr)
        self.assertIn("config.worktree", refused.stderr)
        self.assertFalse(os.path.lexists(out), "refused before any output was written")

        clone = os.path.join(self.tmp, "plain src")
        run(["git", "clone", "--quiet", "--no-hardlinks", "--", self.repo, clone], check=True)
        run(["git", "-C", clone, "checkout", "--quiet", "--detach", self.commit], check=True)
        self.assertEqual(run(["git", "-C", clone, "rev-parse", "--verify", "HEAD^{commit}"], check=True).stdout.strip(), self.commit)
        self.assertFalse(os.path.lexists(os.path.join(clone, ".git", "config.worktree")))
        built = run([sys.executable, "-B", TOOL, "build", "--binary", self.binary, "--source", clone,
                     "--commit", self.commit, "--out", out], env=dict(os.environ, HOME=self.pkg_home))
        self.assertEqual(built.returncode, 0, built.stderr)
        archive = json.loads(built.stdout)["archive"]
        self.assertTrue(archive.startswith(out + os.sep), "output stays outside the clone")
        ok = run([sys.executable, "-B", TOOL, "verify", archive])
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertEqual(json.loads(ok.stdout)["commit"], self.commit)

    # ---------- C2: bundle ownership/modes and private staging ----------
    def scripted_bundle(self, archive=None):
        """A verified bundle whose bin/brain is a script recording how it ran."""
        top = self.unpack(archive or self.built()["archive"])
        script = b'#!/bin/sh\nprintf "%s\\n" "$0" > "$HOME/script-ran"\n'
        write(os.path.join(top, "bin", "brain"), script, "wb")
        os.chmod(os.path.join(top, "bin", "brain"), 0o755)
        sums = read(os.path.join(top, "SHA256SUMS"), "r").splitlines(True)
        digest = hashlib.sha256(script).hexdigest()
        write(os.path.join(top, "SHA256SUMS"), "".join(f"{digest}  bin/brain\n" if l.endswith("  bin/brain\n") else l for l in sums))
        return top

    def test_c2_installer_runs_a_private_verified_copy(self):
        top = self.scripted_bundle()
        home = self.home()
        self.chown(top, home)
        result = self.install(top, home)
        self.assertEqual(result.returncode, 0, result.stderr)
        ran = read(os.path.join(home, "script-ran"), "r").strip()
        self.assertTrue(os.path.basename(os.path.dirname(ran)).startswith("brain-ed-install."), ran)
        self.assertFalse(ran.startswith(top), "the bundle path itself is not executed")
        self.assertFalse(os.path.exists(os.path.dirname(ran)), "the private staging directory is removed")

    def test_c2_writable_or_foreign_bundle_entries_are_refused_before_anything_runs(self):
        cases = {"group-writable file": lambda t: os.chmod(os.path.join(t, "docs", "INSTALL.md"), 0o664),
                 "world-writable top": lambda t: os.chmod(t, 0o757),
                 "group-writable bin dir": lambda t: os.chmod(os.path.join(t, "bin"), 0o775)}
        if AS_ROOT:
            cases["foreign-owned file"] = lambda t: os.lchown(os.path.join(t, "LICENSE"), 0, 0)
            cases["foreign-owned top"] = lambda t: os.lchown(t, 0, 0)
        archive = self.built()["archive"]  # built once: the packager never overwrites an archive
        for label, damage in cases.items():
            with self.subTest(label):
                shutil.rmtree(os.path.join(self.tmp, "unpacked"), True)
                top = self.scripted_bundle(archive)
                home = self.home() if not os.path.exists(os.path.join(self.tmp, "home with space")) else os.path.join(self.tmp, "home with space")
                self.chown(top, home)
                damage(top)
                before = snapshot(home)
                result = self.install(top, home, "--dry-run")
                self.assertNotEqual(result.returncode, 0, label)
                self.assertIn("not owned by you or group/world-writable", result.stderr, label)
                self.assertFalse(os.path.exists(os.path.join(home, "script-ran")), label + ": the bundled binary never ran")
                self.assertEqual(snapshot(home), before, label)

    def test_nul_in_sha256sums_is_refused_before_the_binary_runs(self):
        archive = self.built()["archive"]
        top = self.scripted_bundle(archive)
        home = self.home()
        self.chown(top, home)
        ok = self.install(top, home, "--dry-run")
        self.assertEqual(ok.returncode, 0, "control: a valid bundle still dry-runs: " + ok.stderr)
        self.assertTrue(os.path.exists(os.path.join(home, "script-ran")), "control: the scripted binary ran")
        wording = ok.stderr
        os.remove(os.path.join(home, "script-ran"))
        shutil.rmtree(os.path.join(self.tmp, "unpacked"))
        top = self.scripted_bundle(archive)
        sums = os.path.join(top, "SHA256SUMS")
        body = read(sums)
        row = body.index(b"  bin/brain\n")
        write(sums, body[:row] + b"\0" + body[row:], "wb")  # NUL just before the two spaces
        self.chown(top, home)
        before = snapshot(home)
        result = self.install(top, home, "--dry-run")
        self.assertFalse(os.path.exists(os.path.join(home, "script-ran")), "the bundled binary never ran")
        self.assertEqual(snapshot(home), before, "HOME unchanged")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("NUL", result.stderr)
        self.assertIn("no installation changes", wording, "dry-run wording")

    @unittest.skipUnless(AS_ROOT, "root refusal is checked only when the suite runs as root")
    def test_root_is_refused(self):
        top = self.unpack(self.built()["archive"])
        result = run([os.path.join(top, "install-linux.sh"), "--dry-run"], env={"PATH": "/usr/bin:/bin", "HOME": "/root"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to run as root", result.stderr)


def load_tool():
    spec = importlib.util.spec_from_file_location("package_linux", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HeldOutputDirectory(unittest.TestCase):
    """write_no_clobber checks the directory it actually holds, including one
    created or loosened by someone else after the preflight."""

    def setUp(self):
        self.tool = load_tool()
        self.tmp = tempfile.mkdtemp(prefix="out test ")
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def test_good_directory_then_no_clobber(self):
        out = os.path.join(self.tmp, "fresh")  # does not exist yet: created at write time
        target = self.tool.write_no_clobber(out, "a.tar.gz", b"payload")
        self.assertEqual(read(target), b"payload")
        self.assertEqual(os.stat(out).st_mode & 0o022, 0)
        with self.assertRaises(self.tool.Refused):
            self.tool.write_no_clobber(out, "a.tar.gz", b"other")
        self.assertEqual(read(target), b"payload")
        self.assertEqual(sorted(os.listdir(out)), ["a.tar.gz"])

    def test_loosened_race_created_foreign_or_replaced_directories_are_refused(self):
        cases = []
        loose = os.path.join(self.tmp, "loose"); os.mkdir(loose); os.chmod(loose, 0o775); cases.append(loose)
        race = os.path.join(self.tmp, "race"); os.mkdir(race); os.chmod(race, 0o777); cases.append(race)
        if AS_ROOT:
            foreign = os.path.join(self.tmp, "foreign"); os.mkdir(foreign, 0o755); os.chown(foreign, NOBODY, NOBODY); cases.append(foreign)
        real = os.path.join(self.tmp, "real"); os.mkdir(real, 0o755)
        swapped = os.path.join(self.tmp, "swapped"); os.symlink(real, swapped); cases.append(swapped)
        for directory in cases:
            with self.subTest(os.path.basename(directory)):
                with self.assertRaises(self.tool.Refused):
                    self.tool.write_no_clobber(directory, "a.tar.gz", b"payload")
                self.assertEqual(os.listdir(directory), [], "nothing staged or published")
        self.assertEqual(os.listdir(real), [])


if __name__ == "__main__":
    unittest.main()
