#!/usr/bin/env python3
"""Measure a v2 trace/ZIP, validate Android callsites and idle invariants, compare a baseline."""
import argparse
from collections import Counter
import json
from pathlib import Path
import zipfile


def records(path):
    if path.suffix == '.zip':
        with zipfile.ZipFile(path) as archive:
            chunks = [archive.read(name) for name in archive.namelist() if name.endswith('.jsonl')]
    else:
        files = sorted(path.rglob('*.jsonl')) if path.is_dir() else [path]
        chunks = [file.read_bytes() for file in files]
    result = []
    for chunk in chunks:
        for line in chunk.splitlines():
            event = json.loads(line)
            if event.get('schema_version') == 2 and event.get('side') == 'android':
                result.append((event, len(line) + 1))
    return sorted(result, key=lambda item: item[0]['wall_ms'])


def measure(items):
    if not items:
        raise ValueError('No Android Trace v2 events found')
    start_event = next((v for v, _ in items if v['event'] == 'IDLE_VALIDATION_BEGIN'), None)
    start = start_event['wall_ms'] if start_event else None
    end_event = next((v for v, _ in reversed(items) if v['event'] == 'IDLE_VALIDATION_END'), None)
    end = end_event['wall_ms'] if end_event else None
    interval = [(v, size) for v, size in items if (start is None or v['wall_ms'] >= start) and (end is None or v['wall_ms'] <= end)]
    counts = Counter(v['event'] for v, _ in interval)
    minutes = max((interval[-1][0]['wall_ms'] - interval[0][0]['wall_ms']) / 60000, 1/60000)
    old_empty = sum(v['event'] == 'POLL_WAKE_RESULT' and v['fields'].get('result') == 'no_wake' for v, _ in interval)
    polls = sum(v['fields'].get('poll_count', 0) for v, _ in interval if v['event'] == 'POLL_WAKE_IDLE_SUMMARY')
    nonempty = sum(v['event'] == 'POLL_WAKE_RESULT' and (v['fields'].get('kind') in ('share', 'transport_invalid') or v['fields'].get('result') in ('share_wake', 'invalidated')) for v, _ in interval)
    source_events = [v for v, _ in items if str(v.get('source', {}).get('file', '')).endswith('.kt')]
    bad_sources = [v for v in source_events if not v['source'].get('line') or str(v['source'].get('function', '')).startswith('$r8$')]
    if not source_events:
        raise ValueError('No Kotlin callsites found; a native-only trace cannot validate APK locations')
    starts = Counter(v['context'].get('round_id') for v, _ in items if v['event'] == 'ROUND_START')
    ends = Counter(v['context'].get('round_id') for v, _ in items if v['event'] == 'ROUND_END')
    exact_polls = start_event and end_event and 'poll_count' in start_event['fields'] and 'poll_count' in end_event['fields']
    poll_count = end_event['fields']['poll_count'] - start_event['fields']['poll_count'] if exact_polls else polls + old_empty + nonempty
    idle_keys = ('idle_waits', 'local_wakes', 'remote_wakes', 'network_wakes', 'cancel_wakes', 'timeouts', 'polls', 'empty_poll_count')
    start_idle = start_event['fields'].get('idle', {}) if start_event else {}
    end_idle = end_event['fields'].get('idle', {}) if end_event else {}
    event_driven = all(key in start_idle and key in end_idle for key in idle_keys)
    idle_delta = {key: end_idle[key] - start_idle[key] if event_driven else None for key in idle_keys}
    # Do not let a zero native counter conceal explicit empty-result regressions.
    explicit_empty = sum(v['event'] == 'POLL_WAKE_RESULT' and v['fields'].get('kind') == 'none' for v, _ in interval)
    if event_driven:
        idle_delta['empty_poll_count'] = max(idle_delta['empty_poll_count'], old_empty + explicit_empty)
    connection_stages = Counter((v['context'].get('connection_id'), v['event']) for v, _ in items
                                if v['event'] in ('CONNECTION_AUTHENTICATED', 'PERSISTENT_CONNECTION_INSTALLED', 'CONNECTION_ESTABLISHED'))
    metrics = {
        'duration_minutes': round(minutes, 2), 'poll_count': poll_count,
        'poll_count_note': 'exact native counter between idle markers' if exact_polls else 'aggregated polls; boundary summaries may overlap or omit up to 30s of the idle window',
        'connections_authenticated': counts['CONNECTION_AUTHENTICATED'] + counts['CONNECTION_ESTABLISHED'],
        'persistent_connections_installed': counts['PERSISTENT_CONNECTION_INSTALLED'],
        'connections_cleared': counts['CONNECTION_CLEARED'], 'rounds': counts['ROUND_START'],
        'full_scan_fallbacks': counts['FULL_SCAN_FALLBACK'], 'audits': counts['AUDIT_SCHEDULED'],
        'interrupted_retries': counts['IO_INTERRUPTED_RETRY'],
        'unexpected_errors': sum(v.get('level') == 'error' for v, _ in interval),
        'transfers': counts['TRANSFER_START'],
        'poll_trace_events': sum(count for name, count in counts.items() if name.startswith('POLL_WAKE')),
        'trace_bytes_per_minute': round(sum(size for _, size in interval) / minutes),
        'duplicate_connection_stages': sum(count - 1 for count in connection_stages.values() if count > 1),
        'bad_kotlin_sources': len(bad_sources), 'round_lifecycle_balanced': starts == ends and None not in starts,
        'cpu_wakeups': None, 'event_driven_idle': event_driven, **idle_delta,
    }
    return metrics, interval


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    parser.add_argument('--before', type=Path)
    parser.add_argument('--assert-idle', action='store_true', help='Use only a settled, stable-network idle recording')
    args = parser.parse_args()
    metrics, interval = measure(records(args.trace))
    output = {'after': metrics}
    if args.before:
        before, _ = measure(records(args.before))
        output['before'] = before
        output['delta'] = {key: metrics[key] - before[key] for key in metrics
                           if isinstance(metrics[key], (int, float)) and not isinstance(metrics[key], bool)}
    print(json.dumps(output, indent=2, ensure_ascii=False))
    failures = []
    if metrics['duplicate_connection_stages']: failures.append('Repeated connection stage for the same connection_id')
    if metrics['bad_kotlin_sources']: failures.append('Invalid Kotlin source locations')
    if not metrics['round_lifecycle_balanced']: failures.append('Round start/end mismatch (flush after a completed round)')
    if args.assert_idle:
        audit_times = [event['wall_ms'] for event, _ in interval if event['event'] == 'AUDIT_SCHEDULED']
        if any(later - earlier < 59000 for earlier, later in zip(audit_times, audit_times[1:])):
            failures.append('Audits scheduled sooner than the 60s policy')
        for key in ('transfers', 'unexpected_errors', 'connections_cleared'):
            if metrics[key]: failures.append(f'Idle {key}={metrics[key]}')
        for event, _ in interval:
            if event['event'] == 'WAKE_REQUESTED' and event['fields'].get('source') not in ('NETWORK_RECONNECT',):
                failures.append('Filesystem wake during unchanged idle scenario')
            if event['event'] == 'FULL_SCAN_FALLBACK' and event['fields'].get('reason') not in ('full_scan_requested',):
                failures.append('Unexpected full scan fallback')
        if metrics['event_driven_idle']:
            if metrics['empty_poll_count'] != 0:
                failures.append('Empty periodic polls in event-driven idle')
            if metrics['duration_minutes'] >= 9 and metrics['poll_count'] > metrics['audits'] + 5:
                failures.append('Idle waits exceed audits plus boundary allowance; check for periodic polling')
        elif metrics['duration_minutes'] >= 9 and metrics['poll_count'] > metrics['duration_minutes'] * 90:
            failures.append('Polling volume above 1.5/s')
    if failures:
        raise SystemExit('\n'.join(failures))


if __name__ == '__main__':
    main()
