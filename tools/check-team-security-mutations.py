#!/usr/bin/env python3
"""Kill six M3 attack mutations in a disposable source copy, never patch the worktree."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def replace_once(text, before, after):
    if text.count(before) != 1:
        raise AssertionError(f"mutation anchor count {text.count(before)}: {before}")
    return text.replace(before, after, 1)


def replace_body(text, signature, body):
    start = text.index("{", text.index(signature))
    depth = 1
    end = start + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[:start + 1] + body + text[end - 1:]


CASES = [
    ("replica tamper", "brain-core/src/team/keys.rs",
     lambda s: replace_body(s, "pub fn verify(", " let _ = (public, message, signature); Ok(()) "),
     "attack_replica_tamper_with_recomputed_digests_is_rejected"),
    ("foreign machine key", "brain-core/src/team/store.rs",
     lambda s: replace_body(s, "fn team_verify_replica(", " let _ = (machine, replica); Ok(()) "),
     "attack_foreign_key_signature_cannot_replace_pinned_machine"),
    ("replay after rekey", "brain-core/src/team/store.rs",
     lambda s: replace_body(s, "fn team_verify_replica(", " let _ = (machine, replica); Ok(()) "),
     "attack_old_snapshot_after_rekey_is_rejected_including_history"),
    ("project A to B", "brain-core/src/team/security.rs",
     lambda s: replace_once(s, "|| !rights.projects.iter().any(|p| p == project)",
                           "|| (false && !rights.projects.iter().any(|p| p == project))"),
     "attack_manager_project_a_cannot_read_b"),
    ("expired rights", "brain-core/src/team/security.rs",
     lambda s: replace_once(s, "|| rights.expires_at <= now", "|| (false && rights.expires_at <= now)"),
     "attack_expired_token_is_rejected_at_boundary"),
    ("revoked machine", "brain-core/src/network/auth.rs",
     lambda s: replace_once(s, "revoked_at.is_some() || disabled_at.is_some() || now >= not_after",
                           "(false && (revoked_at.is_some() || disabled_at.is_some())) || now >= not_after"),
     "attack_revoked_machine_cannot_authenticate_even_with_signed_rights"),
]


def main():
    env = dict(os.environ, CARGO_BUILD_JOBS="4")
    if not env.get("CARGO_TARGET_DIR"):
        raise SystemExit("set CARGO_TARGET_DIR to the isolated M3 target directory")
    with tempfile.TemporaryDirectory(prefix="brain-m3-mutations-") as temporary:
        tree = Path(temporary) / "source"
        names = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).decode().split("\0")
        # Include new attack suites when invoked before the final commit.
        names += ["tests/test_team_security.py", "tools/check-team-security-mutations.py"]
        for name in sorted(set(filter(None, names))):
            source = ROOT / name
            if source.is_file():
                destination = tree / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)
        files = {case[1] for case in CASES}
        originals = {name: (ROOT / name).read_text() for name in files}
        for label, filename, mutate, test in CASES:
            for name, original in originals.items():
                (tree / name).write_text(mutate(original) if name == filename else original)
            result = subprocess.run(["nice", "-n", "10", "cargo", "test", "--offline", "--locked",
                                     "-p", "brain-core", test, "--", "--test-threads=1"],
                                    cwd=tree, env=env, capture_output=True, text=True, timeout=300)
            if result.returncode != 101 or "test result: FAILED." not in result.stdout:
                raise AssertionError((label, result.returncode, result.stdout[-3000:], result.stderr[-3000:]))
            print("killed - " + label, flush=True)
    print("all 6 team security mutations rejected")


if __name__ == "__main__":
    main()
