#!/usr/bin/env python3
"""Summarize the three JSONL traces without exposing paths or URIs."""
import collections
import json
import pathlib
import sys
import zipfile

root = pathlib.Path(sys.argv[1])
counts = collections.Counter()
reasons = collections.Counter()
audits = collections.Counter()
interesting = {
    "observer_change", "delta_unavailable", "delta_fallback", "manifest_source",
    "physical_audit", "scheduled_audit", "deep_audit", "round_start", "round_end",
    "observer_callback", "observer_change_classified", "full_scan_fallback", "audit_start", "audit_end", "deep_audit_start",
}


def read(lines, source):
    for line in lines:
        try:
            event = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
        name = event.get("event", "").lower()
        context = event.get("context", event)
        fields = event.get("fields", event)
        if name in interesting:
            counts[source, name] += 1
        if name == "delta_unavailable":
            detail = fields.get("detail", fields)
            reasons[detail.get("reason", "unknown") if isinstance(detail, dict) else detail] += 1
        if name in ("scheduled_audit", "deep_audit", "audit_start", "deep_audit_start"):
            audits[context.get("share_id", "unknown")] += 1
        if fields.get("detail") == "dirty_unavailable":
            reasons["rust_dirty_unavailable"] += 1


for chunk in sorted((root / "Latest-trace").glob("*.jsonl")):
    with chunk.open() as stream:
        read(stream, "pc")

pc = root / "performance-trace-pc.jsonl"
if pc.exists():
    with pc.open() as stream:
        read(stream, "pc")
previous = root / "previous-performance-trace-pc.jsonl"
if previous.exists():
    with previous.open() as stream:
        read(stream, "pc_before")
archive = root / "android-traces.zip"
if archive.exists():
    with zipfile.ZipFile(archive) as zip_file:
        for name in zip_file.namelist():
            with zip_file.open(name) as stream:
                read((line.decode("utf-8") for line in stream), name)

summary = {"events": {f"{side}:{name}": count for (side, name), count in sorted(counts.items())},
           "delta_unavailable_reasons": dict(sorted(reasons.items())),
           "audit_share_counts": dict(sorted(audits.items()))}
(root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
