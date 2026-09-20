#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
rowd_root="$PWD"
export JAVA_HOME="${JAVA_HOME:-$rowd_root/.toolchain/jdk}"
export ANDROID_HOME="${ANDROID_HOME:-$rowd_root/.toolchain/android-sdk}"
rowd_ndk="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.2.12479018}"
rowd_toolchain="$rowd_ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
export PATH="$JAVA_HOME/bin:$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$rowd_toolchain/aarch64-linux-android26-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export AR_aarch64_linux_android="$rowd_toolchain/llvm-ar"
rustup target add aarch64-linux-android
cargo build --release -p rowd-android --target aarch64-linux-android
mkdir -p android/app/src/main/jniLibs/arm64-v8a
cp target/aarch64-linux-android/release/librowd_android.so android/app/src/main/jniLibs/arm64-v8a/
"$rowd_root/.toolchain/gradle-8.9/bin/gradle" -p android assembleDebug --console=plain
echo "APK: $rowd_root/android/app/build/outputs/apk/debug/app-debug.apk"
