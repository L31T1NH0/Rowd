#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
rowd_root="$PWD"
adb="$rowd_root/.toolchain/android-sdk/platform-tools/adb"
share_dir="${1:?Usage: bash scripts/diagnostic-adb.sh PC_SHARE_DIRECTORY [OUTPUT_DIRECTORY]}"
output="${2:-/tmp/rowd-diagnostic-$(date +%Y%m%d-%H%M%S)}"
pc_home="${ROWD_HOME:-$HOME/.local/share/rowd}"
test -d "$share_dir"
test -f "$pc_home/.rowd/device.json"
for attempt in 1 2 3 4 5; do
    if test "$("$adb" devices | awk '$2 == "device" { count++ } END { print count+0 }')" = 1; then break; fi
    sleep 2
done
test "$("$adb" devices | awk '$2 == "device" { count++ } END { print count+0 }')" = 1
mkdir -p "$output"
if test -f "$pc_home/.rowd/performance-trace-pc.jsonl"; then
    cp "$pc_home/.rowd/performance-trace-pc.jsonl" "$output/previous-performance-trace-pc.jsonl"
fi
if test "${ROWD_SKIP_BUILD:-0}" != 1; then
    bash scripts/build-android.sh diagnostic
    "$HOME/.cargo/bin/cargo" build --release -p rowd --locked
fi
for attempt in 1 2 3; do
    if "$adb" install -r android/app/build/outputs/apk/diagnostic/app-diagnostic.apk; then break; fi
    if test "$attempt" = 3; then exit 1; fi
    sleep 3
done
"$adb" shell am start -n app.rowd/.MainActivity
diagnostic_command() { "$adb" shell am broadcast -n app.rowd/.DiagnosticReceiver -a app.rowd.diagnostic.COMMAND --es command "$1"; }
round_count() { "$adb" logcat -d -s RowdLatency:I | awk '/round_completed_at=/ { n++ } END { print n+0 }'; }
sync_and_wait() {
    local before
    before=$(round_count)
    diagnostic_command sync-now
    for attempt in $(seq 1 180); do
        if test "$(round_count)" -gt "$before"; then return; fi
        sleep 1
    done
    echo "Timed out waiting for an Android sync round" >&2
    return 1
}
wait_pc_file() {
    local filename="$1"
    for attempt in $(seq 1 180); do
        if awk -v needle="$filename" 'index($0, needle) { seen=1 } seen && /round_completed_at=/ { done=1 } END { exit !done }' "$output/pc-terminal.log"; then return; fi
        sleep 1
    done
    echo "Timed out waiting for PC transfer of $filename" >&2
    return 1
}
diagnostic_command trace-start
"$rowd_root/target/release/rowd" run --trace > "$output/pc-terminal.log" 2>&1 &
pc_pid=$!
sleep 2
kill -0 "$pc_pid"
finish() {
    kill "$pc_pid" 2>/dev/null || true
    diagnostic_command trace-flush || true
    diagnostic_command trace-stop || true
    diagnostic_command trace-export || true
    "$adb" logcat -d -s RowdLatency:I Rowd:I RowdDiagnostic:I > "$output/android-logcat.txt" || true
    trace_zip=$("$adb" shell 'ls -t /sdcard/Download/rowd-performance-trace-*.zip 2>/dev/null | head -1' | tr -d '\r') || true
    if test -n "$trace_zip"; then "$adb" pull "$trace_zip" "$output/android-traces.zip" || true; fi
    pc_trace="$pc_home/.rowd/performance-trace-pc.jsonl"
    if test -f "$pc_trace"; then cp "$pc_trace" "$output/performance-trace-pc.jsonl"; fi
    python3 scripts/summarize-diagnostic.py "$output" || true
}
trap finish EXIT
"$adb" logcat -c
date +'%s%3N baseline' >> "$output/scenarios.txt"
sync_and_wait
sync_and_wait # Verify the settled state after any pending work from the prior run.
test_prefix="rowd-diagnostic-$(date +%s)"
date +'%s%3N one_file' >> "$output/scenarios.txt"
printf 'one diagnostic file\n' > "$share_dir/$test_prefix-one.txt"
sleep 2
sync_and_wait
wait_pc_file "$test_prefix-one.txt"
date +'%s%3N five_files' >> "$output/scenarios.txt"
for index in 1 2 3 4 5; do printf 'diagnostic file %s\n' "$index" > "$share_dir/$test_prefix-many-$index.txt"; done
sleep 2
sync_and_wait
wait_pc_file "$test_prefix-many-"
date +'%s%3N periodic_audit' >> "$output/scenarios.txt"
sleep 130 # Two periodic audit ticks, plus room for an active round.
