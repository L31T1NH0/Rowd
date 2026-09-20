#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
rowd_tools="$PWD/.toolchain"
mkdir -p "$rowd_tools"
if [ ! -x "$rowd_tools/jdk/bin/java" ]; then
  curl -fL --retry 2 'https://api.adoptium.net/v3/binary/latest/17/ga/linux/x64/jdk/hotspot/normal/eclipse' -o "$rowd_tools/jdk.tar.gz"
  mkdir -p "$rowd_tools/jdk"
  tar -xzf "$rowd_tools/jdk.tar.gz" --strip-components=1 -C "$rowd_tools/jdk"
fi
if [ ! -x "$rowd_tools/android-sdk/cmdline-tools/latest/bin/sdkmanager" ]; then
  curl -fL --retry 2 https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -o "$rowd_tools/android-tools.zip"
  mkdir -p "$rowd_tools/android-sdk/cmdline-tools"
  unzip -q -o "$rowd_tools/android-tools.zip" -d "$rowd_tools/android-sdk/cmdline-tools"
  mv "$rowd_tools/android-sdk/cmdline-tools/cmdline-tools" "$rowd_tools/android-sdk/cmdline-tools/latest"
fi
if [ ! -x "$rowd_tools/gradle-8.9/bin/gradle" ]; then
  curl -fL --retry 2 https://services.gradle.org/distributions/gradle-8.9-bin.zip -o "$rowd_tools/gradle.zip"
  curl -fL --retry 2 https://services.gradle.org/distributions/gradle-8.9-bin.zip.sha256 -o "$rowd_tools/gradle.sha256"
  rowd_expected=$(tr -d '\n\r ' < "$rowd_tools/gradle.sha256")
  rowd_actual=$(sha256sum "$rowd_tools/gradle.zip")
  [ "$rowd_expected" = "${rowd_actual%% *}" ] || { echo 'Gradle checksum mismatch' >&2; exit 1; }
  unzip -q -o "$rowd_tools/gradle.zip" -d "$rowd_tools"
fi
export JAVA_HOME="$rowd_tools/jdk"
export ANDROID_HOME="$rowd_tools/android-sdk"
export PATH="$JAVA_HOME/bin:$PATH"
# Run this script only after accepting the Android SDK licenses interactively.
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --licenses
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" 'platform-tools' 'platforms;android-35' 'build-tools;35.0.0' 'ndk;27.2.12479018'
echo 'Android build tools ready in .toolchain/'

