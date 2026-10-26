# Public source export

The public repository starts with one root commit, `Brain ED 0.2.0`.
Private development history is not copied. Run from a reviewed, committed checkout:

```sh
CARGO_TARGET_DIR="$HOME/projects/brain-ed-target/CX-R82" \
  bash tools/export-public.sh "$HOME/projects/brain-ed-target/public-export-dry"
```

The destination and its sibling `<OUT_DIR>.export-report` must not exist.
Existing destinations are preserved, including failed exports. No push, remote,
installer, model installation or agent integration is run. Cargo uses offline,
locked dependencies with four build jobs and `nice -n 10`. The required Rust
version and dependency cache must already be installed.

All payload bytes and the exclusion policy come from the pinned HEAD through
`git archive`. Uncommitted and untracked files are never copied. Root-relative
patterns in `packaging/public-exclude.txt` use Python `fnmatch` syntax; a trailing
slash excludes a directory recursively. The report lists each excluded file and
its matching rules. Internal stage reports, owner plans and measurements from
private databases are excluded. Public source, synthetic tests, notices and
user documentation remain available.

Set `PUBLIC_EXPORT_AUTHOR_NAME` and `PUBLIC_EXPORT_AUTHOR_EMAIL` for the public
identity. Both author and committer otherwise use `Brain ED Contributors
<contributors@example.invalid>`, independently of private Git identity settings.
The destination has branch `main`, one commit and no remote.

The export runs the release secret scanner in worktree mode and the privacy
gate. Exact reviewed synthetic secret fixtures and source-code references use
the existing blob/path/line/rule allowlist. New unreviewed matches fail. The
privacy gate rejects home-directory paths and overlay addresses outside test
fixtures, and checks known stand identifiers plus the executing machine's
hostname. Additional stand names can be audited with
`python3 tools/check-public-personal.py --stand-name NAME`. This finite policy
is not a guarantee that all forms of personal data can be detected.

After both scans pass, the export builds and runs `cargo test --workspace`, then
performs an offline init/remember/recall quick start in a disposable HOME.
Toolchain and Cargo caches remain available; Brain settings, databases and model
paths resolve under the disposable HOME. Nothing is installed in the user's
Brain directory. The report retains exit codes and logs, source/public commit
IDs, the excluded-file manifest, tracked-file count and byte sizes. Verification
logs stay outside the new repository. A failed check returns a nonzero exit code
and preserves evidence for inspection.
