"""Tests of scripts/check_boundaries.py (D-79)."""
import unittest

import check_boundaries


def metadata(edges, names=check_boundaries.NAMES):
    packages = []
    for name in sorted(names):
        packages.append({
            'id': name,
            'name': name,
            'dependencies': [{'name': target, 'kind': kind} for source, target, kind in edges if source == name],
        })
    return {'workspace_members': sorted(names), 'packages': packages}


class CheckBoundariesTest(unittest.TestCase):
    def test_core_edges_pass(self):
        edges = [(name, 'fox-services-top-core', None) for name in check_boundaries.NAMES if name != 'fox-services-top-core']
        edges.append(('fox-services-top', 'fox-services-top-store', None))
        self.assertEqual(check_boundaries.check(metadata(edges)), [])

    def test_history_may_use_compression_and_nothing_else_may(self):
        self.assertEqual(check_boundaries.check(metadata([('fox-services-top-history', 'fox-services-top-compression', None)])), [])
        errors = check_boundaries.check(metadata([('fox-services-top-ui', 'fox-services-top-compression', None)]))
        self.assertEqual(errors, ['fox-services-top-ui must not depend on fox-services-top-compression'])
        errors = check_boundaries.check(metadata([('fox-services-top-history', 'fox-services-top-store', None)]))
        self.assertEqual(errors, ['fox-services-top-history must not depend on fox-services-top-store'])

    def test_the_manager_uses_neither_compression_nor_store(self):
        errors = check_boundaries.check(metadata([('fox-services-top-manager', 'fox-services-top-compression', None)]))
        self.assertEqual(errors, ['fox-services-top-manager must not depend on fox-services-top-compression'])
        errors = check_boundaries.check(metadata([('fox-services-top-manager', 'fox-services-top-store', None)]))
        self.assertEqual(errors, ['fox-services-top-manager must not depend on fox-services-top-store'])

    def test_pure_components_refuse_drivers_and_sqlite(self):
        for name in ('fox-services-top-state', 'fox-services-top-history', 'fox-services-top-compression'):
            errors = check_boundaries.check(metadata([(name, 'rusqlite', None)]))
            self.assertEqual(errors, [f'{name} must not depend on rusqlite'])

    def test_dev_edges_and_the_crate_list(self):
        self.assertEqual(check_boundaries.check(metadata([('fox-services-top-ui', 'fox-services-top-store', 'dev')])), [])
        missing = check_boundaries.check(metadata([], names=check_boundaries.NAMES - {'fox-services-top-state'}))
        self.assertTrue(missing[0].startswith('workspace names mismatch'), missing)


if __name__ == '__main__':
    unittest.main()
