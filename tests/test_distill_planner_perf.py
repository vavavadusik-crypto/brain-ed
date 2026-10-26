#!/usr/bin/env python3
"""Full-scale synthetic dry-run regression; never reads the real Brain home."""
import os
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
from distill_planner_benchmark import measure

BINARY = os.environ.get('BRAIN_PROBE_BIN')


@unittest.skipUnless(BINARY, 'set BRAIN_PROBE_BIN to a debug brain binary')
class PlannerPerformance(unittest.TestCase):
    def test_full_scale_plan_and_time(self):
        result = measure(BINARY, timeout=45)
        # Frozen output of the pre-E5.9 planner on the same deterministic data.
        self.assertEqual(result['plan'], {
            'accepted': 0, 'bytes': 31602306, 'calls': 1778, 'chunks': 1778,
            'facts_written': 0, 'proposed': 0, 'rejected': {},
            'roles': ['assistant', 'tool_error', 'user'], 'sessions': 889,
            'status': 'dry_run',
        })
        # The interactive target is <10s; allow loaded debug CI machines 30s.
        self.assertLess(result['seconds'], 30, result)
        print(f"synthetic planner: {result['seconds']:.3f}s, 889 sessions, 31115 entries")


if __name__ == '__main__':
    unittest.main()
