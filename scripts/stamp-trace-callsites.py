#!/usr/bin/env python3
"""Refresh explicit Kotlin source locations; --check detects stale stamps before building."""
import argparse
import re
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--check', action='store_true')
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
changed = []
for path in (root / 'android/app/src').rglob('*.kt'):
    source = path.read_text()
    edits = []
    for match in re.finditer(r'\b(PerformanceTrace\.event|(?<!\.)traceEvent|(?<!\.)event)\(', source):
        if re.search(r'fun\s+$', source[max(0, match.start()-12):match.start()]):
            continue
        # Bare event() belongs only to PerformanceTrace itself.
        if match[1] == 'event' and path.name != 'PerformanceTrace.kt':
            continue
        depth, quote, escape = 1, None, False
        index = match.end()
        while depth:
            char = source[index]
            if quote:
                if escape: escape = False
                elif char == '\\': escape = True
                elif char == quote: quote = None
            elif char in ('"', "'"): quote = char
            elif char == '(': depth += 1
            elif char == ')': depth -= 1
            index += 1
        end = index - 1
        body = source[match.end():end]
        line = source.count('\n', 0, match.start()) + 1
        stamp = re.search(r'\bsourceLine\s*=\s*\d+', body)
        if stamp:
            edits.append((match.end()+stamp.start(), match.end()+stamp.end(), f'sourceLine = {line}'))
        elif re.search(r"\bsourceLine\s*=", body):
            continue # helper forwards caller metadata
        else:
            file = '' if match[1] == 'traceEvent' or re.search(r'\bsourceFile\s*=', body) else f', sourceFile = "{path.name}"'
            edits.append((end, end, f'{file}, sourceLine = {line}'))
    for start, end, replacement in reversed(edits):
        source = source[:start] + replacement + source[end:]
    if source != path.read_text():
        changed.append(str(path.relative_to(root)))
        if not args.check: path.write_text(source)
if changed:
    print(('Stale' if args.check else 'Stamped') + ' callsites: ' + ', '.join(changed))
if args.check and changed:
    raise SystemExit(1)
