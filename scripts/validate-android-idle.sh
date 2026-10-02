#!/usr/bin/env bash
# Requires an already paired PC running Rowd and bound Shares with settled contents.
set -euo pipefail
cd "$(dirname "$0")/.."
rowd_adb="$PWD/.toolchain/android-sdk/platform-tools/adb"
rowd_output="${1:-/tmp/rowd-idle-validation}"
rowd_seconds="${ROWD_IDLE_SECONDS:-600}"
rowd_devices=$("$rowd_adb" devices | awk '$2 == "device" { n++ } END { print n+0 }')
test "$rowd_devices" = 1 || { echo 'Connect exactly one Android device/emulator.' >&2; exit 1; }
if test "${ROWD_SKIP_BUILD:-0}" != 1; then bash scripts/build-android.sh diagnostic; fi
"$rowd_adb" install -r android/app/build/outputs/apk/diagnostic/app-diagnostic.apk
mkdir -p "$rowd_output"
command_rowd() { "$rowd_adb" shell am broadcast -n app.rowd/.DiagnosticReceiver -a app.rowd.diagnostic.COMMAND --es command "$1"; }
"$rowd_adb" shell am start -n app.rowd/.MainActivity
command_rowd trace-stop
command_rowd trace-start
command_rowd trace-location-check
command_rowd sync-start
sleep "${ROWD_SETTLE_SECONDS:-30}"
"$rowd_adb" shell dumpsys batterystats app.rowd > "$rowd_output/batterystats-before.txt"
"$rowd_adb" shell dumpsys cpuinfo > "$rowd_output/cpu-before.txt"
command_rowd idle-begin
for ((rowd_elapsed=0; rowd_elapsed<rowd_seconds; rowd_elapsed+=30)); do
    rowd_wait=$((rowd_seconds-rowd_elapsed))
    if test "$rowd_wait" -gt 30; then rowd_wait=30; fi
    if test "${ROWD_SUSPEND:-0}" = 1 && test "$rowd_elapsed" = 300; then
        "$rowd_adb" shell input keyevent 223
        sleep "$rowd_wait"
        "$rowd_adb" shell input keyevent 224
    else sleep "$rowd_wait"; fi
    echo "Idle elapsed: $((rowd_elapsed+rowd_wait)) seconds"
done
command_rowd idle-end
# Wait for any final audit to finish before stopping the trace; service stays running.
sleep "${ROWD_DRAIN_SECONDS:-10}"
command_rowd trace-flush
command_rowd trace-stop
command_rowd trace-export
"$rowd_adb" shell dumpsys batterystats app.rowd > "$rowd_output/batterystats-after.txt"
"$rowd_adb" shell dumpsys cpuinfo > "$rowd_output/cpu-after.txt"
rowd_zip=$("$rowd_adb" shell 'ls -t /sdcard/Download/rowd-performance-trace-*.zip | head -1' | tr -d '\r')
"$rowd_adb" pull "$rowd_zip" "$rowd_output/android-traces.zip"
python3 scripts/validate-idle-trace.py "$rowd_output/android-traces.zip" --assert-idle > "$rowd_output/metrics.json"
cat "$rowd_output/metrics.json"
