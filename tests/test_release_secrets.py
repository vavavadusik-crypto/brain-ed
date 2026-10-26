"""Secret scanner regressions: only synthetic, disposable Git repositories."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release_scanner", ROOT / "tools/check-release-secrets.py")
SCANNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SCANNER)
MASK, RULES = SCANNER.rules()


class Patterns(unittest.TestCase):
    def test_project_vectors_that_change_are_detected(self):
        vectors = json.loads((ROOT / "tests/secret_cases.json").read_text())
        changed = 0
        for index, vector in enumerate(vectors):
            source = "".join(vector["parts"])
            expected = "".join(vector["expected_parts"])
            if source != expected:
                changed += 1
                self.assertTrue(SCANNER.scan(source.encode(), MASK, RULES), f"vector {index}")
        self.assertGreater(changed, 0)

    def test_extra_temporary_aws_ids_and_url_passwords(self):
        source = ("AS" + "IA" + "0" * 16 + " https://example:fixture-password@example.invalid"
                  + " redis://:fixture-password@example.invalid"
                  + " xapp-" + "1-12345-" + "a" * 20
                  + " https://hooks.slack.com/" + "services/T123/B456/fixture789")
        kinds = {kind for _, kind in SCANNER.scan(source.encode(), MASK, RULES)}
        self.assertIn("aws-temporary-key", kinds)
        self.assertIn("url-password", kinds)
        self.assertIn("slack-app-token", kinds)
        self.assertIn("slack-webhook", kinds)
        matches = SCANNER.scan(b"redis://:synthetic@example.invalid", MASK, RULES)
        self.assertIn((1, "url-password"), matches)

    def test_multiline_key_location_and_masked_values(self):
        source = "intro\n-----BEGIN " + "PRIVATE KEY-----\nfixture\n-----END PRIVATE KEY-----"
        self.assertIn((2, "pem-private-key"), SCANNER.scan(source.encode(), MASK, RULES))
        self.assertEqual([], SCANNER.scan(b"Bearer [REDACTED]\npassword=[REDACTED]", MASK, RULES))
        # A marker in the username must not hide a real password match.
        self.assertTrue(SCANNER.scan(b"https://[REDACTED]:fixture@example.invalid", MASK, RULES))
        self.assertTrue(SCANNER.scan(b"-p'fixture[REDACTED]suffix'", MASK, RULES))


class History(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="brain-secret-scanner-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        old_root, old_rules = SCANNER.ROOT, SCANNER.rules
        self.addCleanup(setattr, SCANNER, "ROOT", old_root)
        self.addCleanup(setattr, SCANNER, "rules", old_rules)
        SCANNER.ROOT = self.root
        SCANNER.rules = lambda: (MASK, RULES)
        self.g("init", "--initial-branch=main", "-q")
        (self.root / "safe").write_text("synthetic repository\n")
        self.commit()

    def g(self, *args):
        return subprocess.check_output(
            ["git", "-c", "user.name=Synthetic test", "-c", "user.email=test@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
            cwd=self.root, stderr=subprocess.DEVNULL)

    def commit(self):
        self.g("add", "-A")
        self.g("commit", "-qm", "synthetic test")
        return self.g("rev-parse", "HEAD").decode().strip()

    def test_deleted_binary_marked_blob_on_other_branch_and_annotated_tag(self):
        self.g("checkout", "-qb", "side")
        credential = "gh" + "p_" + "a" * 36
        (self.root / ".gitattributes").write_text("hidden -diff\n")
        (self.root / "hidden").write_bytes(b"\x00" + credential.encode())
        first = self.commit()
        (self.root / "hidden").unlink()
        self.commit()
        self.g("tag", "-a", "fixture", "-m", "sk-" + "b" * 25)
        self.g("tag", "-a", "nested", "fixture", "-m", "safe annotation")
        self.g("tag", "-d", "fixture")
        self.g("checkout", "-q", "main")
        result = SCANNER.audit()
        self.assertTrue(any(r["path"] == "hidden" and r["commit"] == first for r in result["findings"]))
        self.assertTrue(any(r["path"] == "<tag-metadata>" for r in result["findings"]))
        output = json.dumps(result)
        self.assertNotIn(credential, output)
        self.assertNotIn("sk-" + "b" * 25, output)

    def test_env_filename_and_exact_exception_invalidation(self):
        (self.root / ".env").write_text("EXAMPLE=synthetic\n")
        self.commit()
        result = SCANNER.audit()
        row = next(r for r in result["findings"] if r["type"] == "env-file")
        row["reason"] = "Explicit synthetic env-file fixture"
        allowlist = self.root / "exceptions.json"
        allowlist.write_text(json.dumps({"exceptions": [row]}))
        self.assertEqual(0, SCANNER.audit(allowlist=allowlist)["unreviewed"])
        (self.root / ".env").write_text("EXAMPLE=changed-synthetic\n")
        self.commit()
        self.assertEqual(1, SCANNER.audit(allowlist=allowlist)["unreviewed"])

    def test_tags_of_uncommitted_blobs_and_trees_are_scanned(self):
        (self.root / "tag-only").write_text("gh" + "p_" + "c" * 36)
        blob = self.g("hash-object", "-w", "tag-only").decode().strip()
        self.g("tag", "blob-fixture", blob)
        self.g("add", "tag-only")
        tree = self.g("write-tree").decode().strip()
        self.g("tag", "tree-fixture", tree)
        result = SCANNER.audit()
        self.assertTrue(any(r["path"] == "<tag-blob>" for r in result["findings"]))
        self.assertTrue(any(r["path"] == "tag-only" for r in result["findings"]))

    def test_worktree_untracked_files_are_scanned_without_following_symlinks(self):
        (self.root / "untracked").write_text("password=fixture-value-123\n")
        (self.root / "link").symlink_to("/missing/synthetic")
        result = SCANNER.audit(worktree=True)
        self.assertTrue(any(r["path"] == "untracked" for r in result["findings"]))

    def test_shallow_clone_fails_closed_without_printing_matches(self):
        (self.root / ".git/shallow").write_bytes(self.g("rev-parse", "HEAD"))
        with self.assertRaises(ValueError):
            SCANNER.audit()


if __name__ == "__main__":
    unittest.main()
