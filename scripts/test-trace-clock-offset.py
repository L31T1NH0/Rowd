#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('clock', Path(__file__).with_name('trace-clock-offset.py'))
clock = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clock)


def traces(offset=1162, forward=10, backward=10, count=8):
    pc, android = [], []
    def event(name, time, seq, connection, kind, size):
        return {'event': name, 'wall_ms': time, 'seq': seq, 'context': {'connection_id': connection},
                'fields': {'message_type': kind, 'payload_size': size, 'share_id': 'X'}}
    for i in range(count):
        t1 = i * 1000
        t2 = t1 + offset + forward
        t3 = t2 + 30
        t4 = t1 + forward + 30 + backward
        pc.extend([event('PROTOCOL_SEND', t1, i * 2, 'PC', 'Scan', 100 + i),
                   event('PROTOCOL_RECEIVE', t4, i * 2 + 1, 'PC', 'ScanReady', 200 + i)])
        android.extend([event('PROTOCOL_RECEIVE', t2, i * 2, 'Android', 'Scan', 100 + i),
                        event('PROTOCOL_SEND', t3, i * 2 + 1, 'Android', 'ScanReady', 200 + i)])
    return pc, android


class ClockOffsetTest(unittest.TestCase):
    def test_two_way_direction_and_network_delay(self):
        pc, android = traces()
        result = clock.estimate(pc, android)
        self.assertEqual(result['estimated_peer_clock_offset_ms'], 1162)
        self.assertEqual(result['samples'], 8)
        self.assertEqual(result['jitter_ms'], 0)
        self.assertEqual(result['median_network_uncertainty_ms'], 10)
        self.assertEqual(result['direction'], 'android_minus_pc')

    def test_asymmetry_is_explicit_uncertainty_and_negative_offset_is_allowed(self):
        result = clock.estimate(*traces(-900, forward=5, backward=25))
        self.assertEqual(result['estimated_peer_clock_offset_ms'], -910)
        self.assertEqual(result['median_network_uncertainty_ms'], 15)

    def test_insufficient_one_way_or_ambiguous_frames_return_null(self):
        self.assertIsNone(clock.estimate(*traces(count=2))['estimated_peer_clock_offset_ms'])
        pc, android = traces()
        self.assertIsNone(clock.estimate(pc[::2], android[::2])['estimated_peer_clock_offset_ms'])
        for frames in (pc, android):
            for frame in frames:
                frame['fields']['payload_size'] = 100
        self.assertIsNone(clock.estimate(pc, android)['estimated_peer_clock_offset_ms'])

    def test_duplicate_exports_do_not_create_samples_and_missing_frames_are_safe(self):
        pc, android = traces()
        self.assertEqual(clock.estimate(pc + pc, android + android)['samples'], 8)
        android.pop(5)
        result = clock.estimate(pc, android)
        self.assertEqual(result['estimated_peer_clock_offset_ms'], 1162)
        self.assertLess(result['samples'], 8)

    def test_multiple_indistinguishable_connections_are_rejected(self):
        pc, android = traces()
        second_pc = [{**frame, 'context': {'connection_id': 'PC2'}} for frame in pc]
        second_android = [{**frame, 'context': {'connection_id': 'Android2'}} for frame in android]
        self.assertIsNone(clock.estimate(pc + second_pc, android + second_android)['estimated_peer_clock_offset_ms'])


if __name__ == '__main__':
    unittest.main()
