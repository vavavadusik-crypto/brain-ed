import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from measure_archive_rank import exam_scores


class ExamScoringTest(unittest.TestCase):
    def test_only_ids_and_booleans_survive_exam_output(self):
        output = '\n'.join(f'E{i}: hit  period=PRIVATE_SENTINEL top=[PRIVATE_SENTINEL]'
                           for i in range(1, 11))
        scores = exam_scores(output)
        self.assertEqual(scores['hits'], 10)
        self.assertEqual(scores['errors'], 0)
        self.assertNotIn('PRIVATE_SENTINEL', str(scores))

    def test_missing_duplicate_and_failed_exam_rows_are_not_success(self):
        output = '\n'.join(f'E{i}: hit ' for i in range(1, 11))
        for changed in (output.replace('E3:', 'E2:'), output.split('\n', 1)[1]):
            with self.assertRaises(ValueError):
                exam_scores(changed)
        scores = exam_scores(output.replace('E3: hit ', 'E3: ERROR exit=1'))
        self.assertEqual(scores['hits'], 9)
        self.assertEqual(scores['errors'], 1)


if __name__ == '__main__':
    unittest.main()
