#!/usr/bin/env python3
"""Estimate Android-minus-PC wall clock offset from unambiguous two-way protocol exchanges."""
from collections import defaultdict
from statistics import median


def estimate(pc_events, android_events, minimum_samples=3):
    def streams(events, android):
        result = defaultdict(list)
        seen = set()
        for event in events:
            name = event.get('event')
            if name not in ('PROTOCOL_SEND', 'PROTOCOL_RECEIVE'):
                continue
            ctx, fields = event.get('context', {}), event.get('fields', {})
            connection = ctx.get('connection_id')
            if not connection or not isinstance(event.get('wall_ms'), (int, float)):
                continue
            if not fields.get('message_type') or not isinstance(fields.get('payload_size'), int):
                continue
            process = ctx.get('process_instance_id', '')
            key = (process, connection)
            identity = (key, event.get('seq'), event['wall_ms'], name, str(fields))
            if identity in seen:
                continue  # same chunks exported twice
            seen.add(identity)
            # Both streams are expressed in the PC's direction.
            direction = 'send' if (name == 'PROTOCOL_SEND') != android else 'receive'
            signature = (direction, fields['message_type'], fields['payload_size'], fields.get('share_id', ctx.get('share_id')))
            result[key].append((event, signature))
        for frames in result.values():
            frames.sort(key=lambda frame: (frame[0].get('seq', frame[0]['wall_ms']), frame[0]['wall_ms']))
        return result

    pc, peer = streams(pc_events, False), streams(android_events, True)

    def anchors(connections):
        result = defaultdict(list)
        for key, frames in connections.items():
            for index in range(len(frames) - 2):
                result[tuple(frame[1] for frame in frames[index:index + 3])].append((key, index))
        return result

    left, right = anchors(pc), anchors(peer)
    matches = defaultdict(set)
    pc_peers, peer_pcs = defaultdict(set), defaultdict(set)
    for signature, occurrences in left.items():
        other = right.get(signature, [])
        # No ordinal guessing for repeated types/sizes or concurrent connections.
        if len(occurrences) != 1 or len(other) != 1:
            continue
        (pc_key, i), (peer_key, j) = occurrences[0], other[0]
        pc_peers[pc_key].add(peer_key)
        peer_pcs[peer_key].add(pc_key)
        matches[pc_key, peer_key].update((i + n, j + n) for n in range(3))

    samples, bounds = [], []
    for (pc_key, peer_key), pairs in matches.items():
        if len(pc_peers[pc_key]) != 1 or len(peer_pcs[peer_key]) != 1:
            continue
        mapped, inverse = defaultdict(set), defaultdict(set)
        for i, j in pairs:
            mapped[i].add(j)
            inverse[j].add(i)
        if any(len(value) != 1 for value in (*mapped.values(), *inverse.values())):
            continue
        mapping = {i: next(iter(j)) for i, j in mapped.items()}
        if any(b <= a for a, b in zip([mapping[i] for i in sorted(mapping)], [mapping[i] for i in sorted(mapping)][1:])):
            continue
        for i, j in sorted(mapping.items()):
            if mapping.get(i + 1) != j + 1:
                continue
            request, response = pc[pc_key][i:i + 2]
            peer_request, peer_response = peer[peer_key][j:j + 2]
            if request[1][0] != 'send' or response[1][0] != 'receive':
                continue
            # t1 PC send; t2 Android receive; t3 Android send; t4 PC receive.
            t1, t4 = request[0]['wall_ms'], response[0]['wall_ms']
            t2, t3 = peer_request[0]['wall_ms'], peer_response[0]['wall_ms']
            lower, upper = t3 - t4, t2 - t1
            if t4 < t1 or t3 < t2 or lower > upper:
                continue  # clock jump, lost/misordered trace or impossible causal interval
            samples.append((lower + upper) / 2)
            bounds.append((lower, upper))
    if len(samples) < minimum_samples:
        return {'estimated_peer_clock_offset_ms': None, 'samples': len(samples), 'jitter_ms': None,
                'direction': 'android_minus_pc', 'method': 'unique_protocol_triplets_two_way_midpoint'}
    offset = median(samples)
    return {'estimated_peer_clock_offset_ms': round(offset, 3), 'samples': len(samples),
            'jitter_ms': round(median(abs(value - offset) for value in samples), 3),
            'direction': 'android_minus_pc', 'method': 'unique_protocol_triplets_two_way_midpoint',
            'median_network_uncertainty_ms': round(median((hi - lo) / 2 for lo, hi in bounds), 3)}
