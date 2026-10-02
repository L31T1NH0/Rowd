#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location('idle', Path(__file__).with_name('validate-idle-trace.py'))
idle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(idle)

class IdleTraceTest(unittest.TestCase):
    def test_zip_metrics_and_correlated_terminals(self):
        def event(name, time, fields=None, context=None):
            value = {'schema_version': 2, 'side': 'android', 'wall_ms': time, 'event': name,
                     'source': {'file': 'SyncService.kt', 'line': 200, 'function': name},
                     'context': context or {}, 'fields': fields or {}, 'level': 'trace'}
            return json.dumps(value).encode() + b'\n'
        chunk = event('IDLE_VALIDATION_BEGIN', 0, {'poll_count': 100})
        chunk += event('ROUND_START', 60000, context={'round_id': 'X'})
        chunk += event('ROUND_END', 61000, {'result': 'success'}, {'round_id': 'X'})
        chunk += event('POLL_WAKE_IDLE_SUMMARY', 599000, {'poll_count': 590, 'duration_ms': 590000})
        chunk += event('IDLE_VALIDATION_END', 600000, {'poll_count': 700})
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / 'trace.zip'
            with zipfile.ZipFile(trace, 'w') as archive:
                archive.writestr('trace-0001.jsonl', chunk)
            metrics, _ = idle.measure(idle.records(trace))
        self.assertEqual(metrics['poll_count'], 600)
        self.assertEqual(metrics['duration_minutes'], 10)
        self.assertTrue(metrics['round_lifecycle_balanced'])
        self.assertEqual(metrics['bad_kotlin_sources'], 0)
        self.assertEqual(metrics['connections_cleared'], 0)
        self.assertEqual(metrics['transfers'], 0)

    def test_connection_stages_are_distinct_and_not_duplicated(self):
        def stage(name):
            return ({'wall_ms': 1, 'event': name, 'source': {'file': 'SyncService.kt', 'line': 1},
                     'context': {'connection_id': 'C'}, 'fields': {}}, 100)
        items = [stage('CONNECTION_AUTHENTICATED'), stage('PERSISTENT_CONNECTION_INSTALLED')]
        metrics, _ = idle.measure(items)
        self.assertEqual(metrics['duplicate_connection_stages'], 0)
        items.append(stage('PERSISTENT_CONNECTION_INSTALLED'))
        metrics, _ = idle.measure(items)
        self.assertEqual(metrics['duplicate_connection_stages'], 1)

    def test_event_driven_counters_measure_wakes_and_detect_empty_poll_regression(self):
        keys = ('idle_waits', 'local_wakes', 'remote_wakes', 'network_wakes', 'cancel_wakes', 'timeouts', 'polls', 'empty_poll_count')
        def marker(name, time, counters, calls):
            return ({'wall_ms': time, 'event': name, 'source': {'file': 'SyncService.kt', 'line': 1},
                     'context': {}, 'fields': {'idle': dict(zip(keys, counters)), 'poll_count': calls}}, 100)
        items = [marker('IDLE_VALIDATION_BEGIN', 0, [100, 1, 2, 3, 4, 90, 100, 0], 100),
                 marker('IDLE_VALIDATION_END', 600000, [110, 1, 2, 3, 4, 100, 110, 0], 110)]
        metrics, _ = idle.measure(items)
        self.assertTrue(metrics['event_driven_idle'])
        self.assertEqual(metrics['poll_count'], 10)
        self.assertEqual(metrics['idle_waits'], 10)
        self.assertEqual(metrics['timeouts'], 10)
        self.assertEqual(metrics['polls'], 10)
        self.assertEqual(metrics['empty_poll_count'], 0)
        for key in ('local_wakes', 'remote_wakes', 'network_wakes', 'cancel_wakes'):
            self.assertEqual(metrics[key], 0)
        items.insert(1, ({'wall_ms': 300000, 'event': 'POLL_WAKE_RESULT', 'source': {'file': 'lib.rs', 'line': 1},
                          'context': {}, 'fields': {'kind': 'none'}}, 100))
        self.assertEqual(idle.measure(items)[0]['empty_poll_count'], 1)

    def test_native_only_trace_cannot_prove_kotlin_locations(self):
        with self.assertRaisesRegex(ValueError, 'No Kotlin callsites'):
            idle.measure([({'wall_ms': 0, 'event': 'TRACE_START', 'source': {'file': 'trace.rs', 'line': 1}, 'context': {}, 'fields': {}}, 100)])

if __name__ == '__main__':
    unittest.main()
