"""Run focused Rust regressions on broken copies of the current worktree.

No production file is changed. All builds use the caller's CARGO_TARGET_DIR,
which must be the dedicated E4C directory. Fixtures and HOME are temporary.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = Path.home() / 'projects/brain-ed-target/E4C'
assert Path(os.environ['CARGO_TARGET_DIR']).resolve() == TARGET
GRAPH = 'brain-cli/src/graph_view.rs'
UI = 'brain-cli/src/ui.rs'


def replace(source, before, after):
    assert before in source, before
    return source.replace(before, after, 1)


def break_temporal(source, function):
    start = source.index('pub(crate) fn ' + function + '(')
    stop = source.find('\npub(crate) fn ', start + 1)
    if stop < 0:
        stop = len(source)
    section = source[start:stop]
    # Keep parameter slots valid: this mutation restores the original lack of
    # temporal filtering, not an unrelated SQL parameter-count error.
    import re
    section, count = re.subn(
        r'AND v\.(?:recorded_at|valid_from) <= \?3 AND \(v\.(?:superseded_at|valid_to) IS NULL OR \?3 < v\.(?:superseded_at|valid_to)\)',
        'AND ?3 >= 0', section)
    assert count == 2, (function, count)
    return source[:start] + section + source[stop:]


CASES = [
    ('read-only open', GRAPH, lambda s: replace(s, 'brain_core::SqliteStorage::open_read_only(database)', 'brain_core::SqliteStorage::open(database)'), 'viewer_search_does_not_create_a_missing_database'),
    ('access tracking', GRAPH, lambda s: replace(s, '.without_access_tracking()', ''), 'viewer_search_get_preserves_access_and_salience'),
    ('prefix matching', GRAPH, lambda s: replace(s, '.with_prefix_matching()', ''), 'viewer_search_matches_an_incomplete_word'),
    ('empty q rejection', UI, lambda s: replace(s, 'if query.trim().is_empty() {', 'if false {'), 'viewer_search_rejects_blank_and_finds_prefixes'),
    ('node temporal scope', GRAPH, lambda s: break_temporal(s, 'node_document'), 'viewer_excludes_forgotten_and_future_versions'),
    ('graph temporal scope', GRAPH, lambda s: break_temporal(s, 'document'), 'viewer_excludes_forgotten_and_future_versions'),
    ('details temporal scope', GRAPH, lambda s: break_temporal(s, 'details_document'), 'viewer_excludes_forgotten_and_future_versions'),
    ('details id', GRAPH, lambda s: replace(s, 'id,     // stable join key, independent of selection order', 'Value::Null, // broken identity'), 'viewer_details_include_ids_and_stats_report_total_above_limit'),
    ('total above sample limit', GRAPH, lambda s: replace(s, '"total_nodes": total_nodes,', '"total_nodes": rows.len(),'), 'viewer_details_include_ids_and_stats_report_total_above_limit'),
]

with tempfile.TemporaryDirectory(prefix='e4c-rust-mutations-') as directory:
    scratch = Path(directory)
    tree = scratch / 'source'
    tracked = subprocess.check_output(['git', 'ls-files', '-z'], cwd=ROOT).decode().split('\0')
    for name in filter(None, tracked):
        destination = tree / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, destination)
    originals = {name: (tree / name).read_text() for name in (GRAPH, UI)}
    home = scratch / 'home'
    home.mkdir()
    env = dict(os.environ, HOME=str(home), BRAIN_OFFLINE='true',
               CARGO_HOME=os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')),
               RUSTUP_HOME=os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup')))
    for label, name, mutate, test in CASES:
        for filename, text in originals.items():
            (tree / filename).write_text(mutate(text) if filename == name else text)
        result = subprocess.run(['cargo', 'test', '-p', 'brain-cli', '--bin', 'brain', test],
                                cwd=tree, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode != 101 or 'test result: FAILED.' not in result.stdout:
            raise AssertionError(f'{label}: mutant survived or failed outside assertions\n{result.stdout}')
        print('killed - ' + label, flush=True)
    print('all 9 viewer Rust mutations rejected', flush=True)
