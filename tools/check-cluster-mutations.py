"""Reject isolated broken copies of cluster implementations, never edit the worktree."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
GRAPH = 'brain-cli/src/graph_view.rs'
UI = 'brain-cli/src/ui.rs'
APP = 'brain-cli/assets/viewer/app.js'


def change(text, before, after):
    assert before in text, before
    return text.replace(before, after, 1)


def no_temporal(text):
    start = text.index('pub(crate) fn cluster_index(')
    end = text.index('\nimpl ClusterIndex', start)
    import re
    section, count = re.subn(
        r'AND v\.(?:recorded_at|valid_from) <= \?2 AND \(v\.(?:superseded_at|valid_to) IS NULL OR \?2 < v\.(?:superseded_at|valid_to)\)',
        'AND ?2 >= 0', text[start:end])
    assert count == 6
    return text[:start] + section + text[end:]


RUST = [
    ('partition', GRAPH, lambda s: change(s, 'communities.entry(*label).or_default().push(index);', 'if index != 0 { communities.entry(*label).or_default().push(index); }'), 'clusters_cover_every_node_exactly_once_and_repeat_themselves'),
    ('orphan coverage', GRAPH, lambda s: change(s, 'let labels = propagate_labels(&adjacency);', 'let mut labels = propagate_labels(&adjacency); for (i, a) in adjacency.iter().enumerate() { if a.is_empty() { labels[i]=0; } }'), 'orphan_nodes_become_singletons_and_are_not_lost'),
    ('order independence', GRAPH, lambda s: change(s, 'let labels = propagate_labels(&adjacency);', 'let labels = (0..nodes.len() as u32).collect::<Vec<_>>();'), 'the_fold_does_not_depend_on_write_order'),
    ('corridors', GRAPH, lambda s: change(s, 'links.truncate(MAX_CLUSTER_LINKS);', 'links.clear();'), 'a_fold_with_a_corridor_lists_it_and_a_cluster_expands_by_id'),
    ('temporal exclusion', GRAPH, no_temporal, 'clusters_exclude_forgotten_future_and_other_spaces'),
    ('pagination', GRAPH, lambda s: change(s, '.skip(offset)', '.skip(0 * offset)'), 'cluster_pages_reach_every_member_and_selected_graph_stays_scoped'),
    ('scheduled invalidation', GRAPH, lambda s: change(s, 'WHERE t<=?2', 'WHERE t<=0 AND ?2>=0'), 'cached_clusters_follow_scheduled_visibility_without_a_write'),
    ('generation', GRAPH, lambda s: change(s, '"v{version}-t{boundary}-n{}@{}-e{}@{}",', '"constant", /* {version} {boundary} */').replace('row.get::<_, i64>(0)?,\n                row.get::<_, i64>(1)?,\n                row.get::<_, i64>(2)?,\n                row.get::<_, i64>(3)?,',''), 'the_generation_moves_when_the_space_changes'),
    ('bootstrap limit', UI, lambda s: change(s, 'document(&server.database, &server.space, limit)', 'document(&server.database, &server.space, limit.max(4000))'), 'the_cluster_routes_need_the_key_fold_everything_and_refold_on_change'),
    ('cache reuse', UI, lambda s: change(s, 'if *cached == generation {', 'if false && *cached == generation {'), 'the_cluster_routes_need_the_key_fold_everything_and_refold_on_change'),
]
JS = [
    ('expansion', "location.assign(clusterLocation(id));", '/* expansion removed */'),
    ('collapse', "location.assign(clusterLocation(null));", '/* collapse removed */'),
    ('zoom', 'delta < 0 && distance <= 230', 'delta < 0 && distance <= 0'),
    ('overview', 'clusters.slice(page * 4000, (page + 1) * 4000)', 'clusters.slice(0, 1)'),
]
with tempfile.TemporaryDirectory(prefix='e45-mutations-') as directory:
    tree = Path(directory) / 'source'
    for name in filter(None, subprocess.check_output(['git','ls-files','-z'], cwd=ROOT).decode().split('\0')):
        if not (ROOT / name).is_file():
            continue
        destination = tree / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, destination)
    originals = {name: (ROOT / name).read_text() for name in (GRAPH, UI, APP)}
    for label, before, after in JS:
        (tree / APP).write_text(change(originals[APP], before, after))
        result = subprocess.run(['node', str(ROOT / 'tests/test_viewer_clusters.js')], env=dict(os.environ, VIEWER_SOURCE_ROOT=str(tree)), capture_output=True, text=True)
        assert result.returncode == 1 and 'AssertionError' in result.stderr, (label, result.stdout, result.stderr)
        print('killed JS - ' + label, flush=True)
    (tree / APP).write_text(originals[APP])
    for label, name, mutate, test in RUST:
        for filename in (GRAPH, UI):
            (tree / filename).write_text(mutate(originals[filename]) if filename == name else originals[filename])
        result = subprocess.run(['cargo','test','-p','brain-cli','--bin','brain',test], cwd=tree, env=os.environ, capture_output=True, text=True)
        assert result.returncode == 101 and 'test result: FAILED.' in result.stdout, (label, result.stdout, result.stderr[-4000:])
        print('killed Rust - ' + label, flush=True)
print('all 14 cluster mutations rejected')
