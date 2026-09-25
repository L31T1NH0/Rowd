# Rowd

Rowd syncs folders between one Linux PC and one Android phone on the same local network. You choose which folders connect and whether files travel both ways, to Android, or to the PC. The devices transfer files over TLS without a cloud account or relay.

**Rowd 0.5.0 is experimental.** The Rust test suite and Android build pass, and the app has seen use on a real device. We have not documented a full test matrix across Android storage providers, long sessions, network failures, and recovery. Start with disposable folders and keep a separate copy of important files.

## Get Rowd

Download both files from the [latest release](https://github.com/L31T1NH0/Rowd/releases/latest):

| Device | File |
| --- | --- |
| Linux x86_64 | `rowd-v0.5.0-linux-x86_64` |
| Android 8 or newer, ARM64 | `rowd-v0.5.0-android-arm64.apk` |

The release includes `rowd-v0.5.0-SHA256SUMS` so you can check both downloads. The APK uses a debug signing key. Update the PC binary and APK together: version 0.5.0 uses network protocol V8 and rejects older peers.

On Linux, make the downloaded binary executable and start it:

```bash
chmod +x rowd-v0.5.0-linux-x86_64
./rowd-v0.5.0-linux-x86_64
```

Keep both devices on the same LAN. The phone must reach TCP port `43821` on the PC.

## Pair and sync a folder

1. Open the **Device** tab in the Linux terminal app and create an invitation with your PC's LAN address, such as `192.168.1.20:43821`.
2. On Android, tap **Parear por QR** and scan the code. Compare the certificate fingerprint shown on both devices before accepting it.
3. Tap **Escolher pasta para novo Share** on Android. Choose a folder, name the Share, and choose its direction.
4. Open **Requests** in the Linux app, accept the request, and select the matching PC folder.
5. Leave Rowd running on both devices while you want changes to sync.

The Android buttons still use Portuguese labels. You can also create a Share on the PC and bind its Android folder with **Vincular pasta a Share pendente**. The [user guide](docs/USER_GUIDE.md) covers both paths and the CLI.

## What happens to your files

| Change | Behavior |
| --- | --- |
| New or modified file | A trusted watcher event can limit the next round to that path. Rowd verifies content with SHA-256 before transfer and installation. |
| Concurrent edits | Rowd preserves both versions and records a conflict. |
| Delete or rename | Rowd scans the Share to resolve the change. It does not propagate deletions; a removed file can return from the other device. |
| Missed event or lost scan trust | Rowd falls back to an audit. Android can reuse cached hashes during a namespace audit and rehashes files when a deep audit is required. |

Each Share has its own mode, convergence base, conflict records, and recovery state. The devices reuse one TLS connection between rounds while the Android service runs. The Android service may stop under platform limits; Rowd reconnects when it can.

Rowd does not sync symlinks, empty folders, original permissions, or timestamps. It transfers whole files and does not resume a partial file transfer. One PC pairs with one Android device at a time. See [limits and recovery](docs/USER_GUIDE.md#limits-and-recovery) before using a folder that matters to you.

## Build and test

The desktop app is a Rust workspace package:

```bash
cargo build --release -p rowd
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked
```

The Android build needs JDK 17, Android SDK 35, NDK 27.2, and Gradle 8.9. `scripts/build-android.sh` uses the repository's local `.toolchain/` directory. GitHub Actions builds the ARM64 JNI library and a debug APK on each push and pull request.

## Documentation

- [User guide](docs/USER_GUIDE.md): pairing, Share modes, CLI, backup, and recovery.
- [Architecture](docs/ARCHITECTURE.md): protocol, storage, and trust boundaries. This document is in Portuguese.

Rowd is licensed under [MIT](LICENSE).
