import importlib.util
from pathlib import Path
import unittest
import tempfile
import contextlib
import io
import os
import sys
from unittest.mock import patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location('retrieval_run', Path(__file__).with_name('run.py'))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
compare_spec = importlib.util.spec_from_file_location('retrieval_compare', Path(__file__).with_name('compare.py'))
compare = importlib.util.module_from_spec(compare_spec)
compare_spec.loader.exec_module(compare)


class MetricsTest(unittest.TestCase):
    def test_wrong_section_and_duplicates_do_not_inflate_metrics(self):
        expected = [{'path': '/fixture/a.md', 'section': 'Wanted'},
                    {'path': '/fixture/b.md'}]
        hits = [{'source': '/fixture/a.md', 'text': '## Sibling'},
                {'source': '/fixture/a.md', 'text': '# File\n## Wanted\nbody'},
                {'source': '/fixture/a.md', 'text': '## Wanted\nbody'},
                {'source': '/fixture/b.md', 'text': 'body'}]
        score = bench.metrics(hits, expected)
        self.assertEqual(score['recall_at_5'], 1)
        self.assertEqual(score['mrr_at_10'], .5)
        self.assertAlmostEqual(score['ndcg_at_10'],
                               (1/bench.math.log2(3)+1/bench.math.log2(5))/(1+1/bench.math.log2(3)))

    def test_rank_boundaries_and_archive_interference(self):
        hits = [{'source': 'archive', 'text': '## Wanted'}] * 5
        hits += [{'source': '/fixture/a.md', 'text': '## Wanted'}]
        score = bench.metrics(hits, [{'path': '/fixture/a.md', 'section': 'Wanted'}])
        self.assertEqual(score['recall_at_5'], 0)
        self.assertEqual(score['mrr_at_10'], 1/6)
        self.assertEqual(bench.metrics(hits[:1]*11 + hits[5:],
                                     [{'path': '/fixture/a.md'}])['mrr_at_10'], 0)

    def test_archive_cannot_impersonate_a_markdown_source(self):
        self.assertFalse(bench.relevant({'client': 'fixture', 'source': '/fixture/a.md',
                                        'text': '## Wanted'},
                                       {'path': '/fixture/a.md', 'section': 'Wanted'}))

    def test_home_rejects_hard_linked_database(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.brain').mkdir()
            (root / 'shared').touch()
            os.link(root / 'shared', root / '.brain/brain.sqlite')
            with self.assertRaises(ValueError):
                bench.run(SimpleNamespace(home=root))

    def test_heading_in_code_fence_is_not_an_answer_section(self):
        self.assertFalse(bench.relevant({'source': '/fixture/a.md',
                                        'text': '## Sibling\n```md\n## Wanted\n```'},
                                       {'path': '/fixture/a.md', 'section': 'Wanted'}))

    def test_memory_heading_title_omits_answer(self):
        self.assertTrue(bench.relevant({'source': '/fixture/a.md',
                                       'text': '## УСПЕХ: Сборка | **Решение:** private'},
                                      {'path': '/fixture/a.md', 'section': 'УСПЕХ: Сборка'}))

    def test_failed_child_never_leaks_output_and_consumes_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.brain').mkdir()
            (root / '.brain/brain.sqlite').touch()
            binary = root / 'fake-brain'
            binary.write_text('#!/bin/sh\necho PRIVATE_SENTINEL >&2\nexit 1\n')
            binary.chmod(0o700)
            questions = root / 'questions.jsonl'
            questions.write_text(bench.json.dumps({'id': 'Q01', 'question': 'fixture',
                                                  'expected': [{'path': '/fixture/a.md'}]})+'\n')
            args = SimpleNamespace(home=root, binary=binary, questions=questions,
                                   with_archive=False, budget_tokens=12000, timeout=5)
            output = io.StringIO()
            original = bench.subprocess.run
            calls = []
            def child(*a, **kw):
                calls.append(kw['env']['BRAIN_DATABASE'])
                return original(*a, **kw)
            with contextlib.redirect_stdout(output), patch.dict(os.environ, BRAIN_DATABASE='/protected'), \
                    patch.object(bench.subprocess, 'run', side_effect=child):
                self.assertEqual(bench.run(args), 1)
            self.assertEqual(calls, [str(root / '.brain/brain.sqlite')])
            self.assertNotIn('PRIVATE_SENTINEL', output.getvalue())
            self.assertEqual(bench.json.loads(output.getvalue().splitlines()[-1])['errors'], 1)
            with self.assertRaises(FileExistsError):
                bench.run(args)

    def test_continuation_chunk_uses_section_identity(self):
        self.assertTrue(bench.relevant({'source': '/fixture/a.md', 'text': 'continuation',
                                       '_section_label': 'Wanted (part 2/3)'},
                                      {'path': '/fixture/a.md', 'section': 'Wanted'}))
        self.assertFalse(bench.relevant({'source': '/fixture/a.md', 'text': 'continuation',
                                        '_section_label': 'Sibling (part 2/3)'},
                                       {'path': '/fixture/a.md', 'section': 'Wanted'}))

    def test_dataset_schema(self):
        rows = [bench.json.loads(s) for s in Path(__file__).with_name('questions.jsonl').read_text().splitlines()]
        bench.validate(rows)
        self.assertEqual(len(rows), 50)
        self.assertEqual({r['language'] for r in rows}, {'ru', 'en'})
        self.assertTrue(all(set(r) == {'id','language','form','question','expected'} for r in rows))

    def test_matrix_refuses_reused_cargo_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = root / 'bins'
            binaries.mkdir()
            for name in ('live', 'morph', 'snowball', 'sections', 'combined'):
                (binaries / name).write_bytes(b'identical-stale-build')
            output = io.StringIO()
            args = ['compare.py', '--base', str(root/'base'), '--sections', str(root/'sections'),
                    '--bins', str(binaries), '--output', str(root/'output')]
            with patch.object(sys, 'argv', args), contextlib.redirect_stdout(output):
                self.assertEqual(compare.main(), 1)
            self.assertEqual(bench.json.loads(output.getvalue())['duplicate_binaries'], 1)
            self.assertFalse((root/'output/live-core').exists())

    def test_corpus_comparison_stays_in_the_bench_space(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'.brain').mkdir()
            with bench.sqlite3.connect(root/'.brain/brain.sqlite') as db:
                db.execute('CREATE TABLE node_versions(source,body,superseded_at,metadata,space_id)')
                for space in ('default', 'another-space'):
                    for section in (None, 0):
                        metadata = bench.json.dumps({'imported_from':'markdown', 'section':section})
                        db.execute('INSERT INTO node_versions VALUES(?,?,NULL,?,?)',
                                   ('/fixture/'+space+'.md', 'synthetic', metadata, space))
            self.assertEqual(set(compare.parents(root)), {'/fixture/default.md'})
            self.assertEqual(compare.section_count(root), 1)

    def test_resume_reuses_complete_scores_and_rejects_changed_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = root/'bins'
            binaries.mkdir()
            fixtures = [root/'base', root/'sections']
            for fixture in fixtures:
                (fixture/'.brain/archive').mkdir(parents=True)
                (fixture/'.brain/brain.sqlite').write_bytes(b'fixture')
                (fixture/'.brain/settings.json').write_bytes(b'{}')
                (fixture/'.brain/archive/sessions.sqlite').write_bytes(b'fixture')
            names = ('live', 'morph', 'snowball', 'sections', 'combined')
            for name in names:
                (binaries/name).write_text(name)
            output = root/'output'
            output.mkdir()
            scripts = Path(__file__).parent
            ids = [bench.json.loads(line)['id'] for line in (scripts/'questions.jsonl').read_text().splitlines()]
            summary = {'errors': 0, 'questions': len(ids)}
            manifest = {
                'parent_documents': 1, 'section_nodes': {'base': 1, 'sections': 2},
                'parent_content_sha256': bench.hashlib.sha256(bench.json.dumps({'fixture': 'hash'}, sort_keys=True).encode()).hexdigest(),
                'questions_sha256': compare.digest(scripts/'questions.jsonl'),
                'runner_sha256': compare.digest(scripts/'run.py'),
                'settings_sha256': compare.digest(fixtures[0]/'.brain/settings.json'),
                'archive_sha256': compare.digest(fixtures[0]/'.brain/archive/sessions.sqlite'),
                'variants': {}}
            for name in names:
                manifest['variants'][name] = {
                    'binary_sha256': compare.digest(binaries/name),
                    'database_sha256': compare.digest(fixtures[0]/'.brain/brain.sqlite'),
                    'scores': {mode: summary for mode in ('core', 'archive')}}
                for mode in ('core', 'archive'):
                    rows = [{'id': q, 'error': 0} for q in ids] + [summary]
                    (output/f'{name}-{mode}.jsonl').write_text(''.join(bench.json.dumps(row)+'\n' for row in rows))
            (output/'manifest.json').write_text(bench.json.dumps(manifest))
            args = ['compare.py', '--base', str(fixtures[0]), '--sections', str(fixtures[1]),
                    '--bins', str(binaries), '--output', str(output), '--resume']
            with patch.object(sys, 'argv', args), contextlib.redirect_stdout(io.StringIO()), \
                    patch.object(compare, 'parents', return_value={'fixture': 'hash'}), \
                    patch.object(compare, 'section_count', side_effect=lambda h: 1 if h == fixtures[0] else 2), \
                    patch.object(compare.subprocess, 'run') as child:
                self.assertEqual(compare.main(), 0)
                child.assert_not_called()
                (binaries/'live').write_text('changed')
                with self.assertRaisesRegex(ValueError, 'resume variant changed'):
                    compare.main()
                child.assert_not_called()


if __name__ == '__main__':
    unittest.main()
