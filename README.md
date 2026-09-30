# <img src="assets/icons/rowd-pc.png" alt="Rowd icon" width="40"> Rowd

> [!WARNING]
> **Unstable alpha (0.7.0).** Bugs may interrupt sync or require manual recovery. Do not rely on Rowd as the only copy of important files. Start with disposable folders and keep an independent backup.

Rowd syncs folders between one Linux PC and one Android phone on the same local network. You choose which folders connect and whether files travel both ways, to Android, or to the PC. The devices transfer files over TLS without a cloud account or relay.

The Rust test suite and Android build pass, and the app has seen use on a real device. We have not documented a full test matrix across Android storage providers, long sessions, network failures, and recovery.

## Get Rowd

Download both files from the [0.7.0 alpha release](https://github.com/L31T1NH0/Rowd/releases/tag/v0.7.0-alpha):

| Device | File |
| --- | --- |
| Linux x86_64 | `rowd-v0.7.0-alpha-linux-x86_64` |
| Android 8 or newer, ARM64 | `rowd-v0.7.0-alpha-android-arm64.apk` |

The release includes `rowd-v0.7.0-alpha-SHA256SUMS` so you can check both downloads. The Android release APK is signed with the debug key. Update the PC binary and APK together to use pairing discovery; both builds use sync protocol V12.

On Linux, make the downloaded binary executable and start it:

```bash
chmod +x rowd-v0.7.0-alpha-linux-x86_64
./rowd-v0.7.0-alpha-linux-x86_64
```

Keep both devices on the same LAN. The phone must reach TCP port `43821`. Endpoint discovery uses multicast UDP `239.255.42.99:43822`; initial pairing discovery uses port `43823`. If endpoint multicast is blocked, Rowd tries the address stored in the invitation.

## Pair and sync a folder

1. Open **Conectar celular** in the Linux terminal app, then choose **Encontrar na rede** or **Mostrar QR**. The CLI equivalent for generating a QR is `rowd pair`; `--address 192.168.1.20:43821` sets a fallback address.
2. On Android, choose **Encontrar na rede**, **Escanear QR**, or **Importar convite**. For network pairing, compare the six-digit code on both devices and approve it on the PC. For QR, compare the certificate fingerprint before accepting.
3. Tap **Escolher pasta para novo Share** on Android. Choose a folder, name the Share, and choose its direction.
4. Open **Requests** in the Linux app, accept the request, and select the matching PC folder.
5. Leave Rowd running on both devices while you want changes to sync.

The Android buttons still use Portuguese labels. You can also create a Share on the PC and bind its Android folder with **Vincular pasta a Share pendente**. The [user guide](docs/USER_GUIDE.md) covers both paths and the CLI.

## Linux daemon (development version)

After configuring Rowd, `rowd daemon start` starts the sync server in the background for the current `ROWD_HOME`. It survives closing the terminal. `rowd daemon stop` shuts it down cleanly, and `rowd daemon restart` preserves manual or systemd launch mode. `rowd daemon run` runs in the foreground. The existing `rowd run` command remains available.

Use `rowd status`, `rowd logs`, and `rowd events` to inspect it. Add `-f` to logs or events to follow their local IPC streams; Ctrl+C closes only the client. Logs and JSONL events are stored under `$ROWD_HOME/.rowd/logs/` with simple 1 MiB rotation. The daemon socket is private under `$XDG_RUNTIME_DIR/rowd/`, or `$ROWD_HOME/.rowd/run/` if no runtime directory exists. IPC uses a separate version 1 JSON-lines protocol over a Unix socket.

`rowd autostart enable` writes a user unit in `~/.config/systemd/user/rowd.service` and enables it for future user sessions. It does not start or stop the current process. `rowd autostart disable` only disables future starts; `rowd autostart status` shows the unit and linger state. With linger disabled, a user service normally starts after login rather than before login. `rowd daemon start` alone does not configure autostart.

While a daemon is active, the current TUI refuses to open because its controls still operate the local app directly. Use the CLI or stop the daemon before opening the TUI. The daemon IPC is the future path for TUI integration.

## Performance trace

For synchronization diagnostics, run `rowd run --trace` on the PC. In the terminal app, press `t` on the **Device** tab to toggle tracing. On Android, enable **Trace de desempenho** on the main screen. Each device writes its own trace; use **Exportar trace** on Android to save both Android traces in a ZIP. See the [trace guide](docs/USER_GUIDE.md#performance-trace) for file locations and details.

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

The Android build needs JDK 17, Android SDK 35, NDK 27.2, and Gradle 8.9. `scripts/build-android.sh` uses the repository's local `.toolchain/` directory. Local and GitHub Actions builds produce a release APK; the debug variant is disabled because it has serious performance costs.

## Documentation

- [User guide](docs/USER_GUIDE.md): pairing, Share modes, CLI, backup, and recovery.
- [Architecture](docs/ARCHITECTURE.md): protocol, storage, and trust boundaries. This document is in Portuguese.

Rowd is licensed under [MIT](LICENSE).
