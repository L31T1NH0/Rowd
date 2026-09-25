# Rowd user guide

Rowd links one Linux PC and one Android phone over a local network. A **Share** connects one folder on each device. You can create several Shares and set a direction for each one.

This guide describes Rowd 0.5.0 and network protocol V8. Update the Linux binary and Android APK together. The Android app currently uses Portuguese button labels; this guide quotes them so you can find them on screen.

## Install and pair

Download `rowd-v0.5.0-linux-x86_64` and `rowd-v0.5.0-android-arm64.apk` from the [0.5.0 release](https://github.com/L31T1NH0/Rowd/releases/tag/v0.5.0). The APK supports Android 8 or newer on ARM64 and uses a debug signing key. Use the release's `SHA256SUMS` file if you want to verify the downloads.

On Linux:

```bash
chmod +x rowd-v0.5.0-linux-x86_64
./rowd-v0.5.0-linux-x86_64
```

The terminal app starts the server. Keep it open and allow the phone to reach TCP port `43821` on the PC. Both devices need to be on the same LAN.

Open the **Device** tab in the terminal app. Start pairing, enter the PC's reachable LAN address, and show the QR code. On Android, tap **Parear por QR**. Compare the certificate fingerprint displayed on both devices before accepting. The invitation contains credentials; treat its QR code, text, and SVG copy as private.

You can create an invitation from the CLI:

```bash
./rowd pair --address 192.168.1.20:43821 --invite /tmp/rowd-invite.txt
```

The first authenticated Android identity becomes the PC's paired device. Rowd rejects a second device until you unlink or revoke the first one.

## Create a Share

### Start on Android

1. Tap **Escolher pasta para novo Share** and choose a folder through Android's file picker.
2. Give the Share a name and select its direction.
3. Open **Requests** in the Linux terminal app. Accept the request and enter the absolute path of the matching PC folder.

The phone keeps the request while the PC is offline. You can also review or cancel it under **Solicitações e decisões**. A repeated request keeps its original ID and does not create another Share.

The CLI can accept or reject a request:

```bash
./rowd request list
./rowd request accept REQUEST_ID --folder "$HOME/Documents/Notes"
./rowd request reject REQUEST_ID
```

### Start on Linux

Add a Share in the terminal app, or run:

```bash
./rowd share add --name Notes --folder "$HOME/Documents/Notes" --mode bidirectional
```

After the phone connects, tap **Vincular pasta a Share pendente** on Android and select the matching folder. Rowd keeps the Share's identity when you rename it. It does not guess an Android path from the PC path.

## Choose a direction

| Mode | Transfer direction |
| --- | --- |
| `bidirectional` | PC ↔ Android |
| `to_android` | PC → Android |
| `to_pc` | Android → PC |

If both devices change the same path before they converge, Rowd keeps the PC version at the original path and saves the Android version under `Rowd Conflicts/`. Directional modes keep edits made on the other side and protect them as conflicts when a transfer would overwrite them.

Deletions do not propagate. If you remove a file from one side, the copy on the other side can restore it. A rename is treated as a removed path and a new path, so check the result before deleting either copy.

## Run and manage Rowd

The Linux terminal app has four tabs: **Shares**, **Requests**, **Recovery**, and **Device**. Use `1` through `4` to switch tabs, `?` for help, and `q` to quit. After setup, the CLI can run without the terminal interface:

```bash
./rowd run
```

The PC watcher and Android content observer wake the next round when they can identify a change. Rowd can focus on one Share or path, then uses SHA-256 to prove file content before transfer. It also audits the namespace for missed events. Android reuses compatible cached hashes during that audit; a deep audit rehashes files after trust loss or on its longer schedule.

The devices keep a TLS connection between rounds while the Android service runs. Network changes or Android service limits can close it. Rowd reconnects and reconciles from the Share's persisted base; an interrupted transfer does not make a session token authoritative.

Useful commands:

```bash
./rowd shares
./rowd share edit SHARE_ID --name Work --mode to_android
./rowd share pause SHARE_ID
./rowd share resume SHARE_ID
./rowd share reindex SHARE_ID
./rowd share sync SHARE_ID
./rowd share remove SHARE_ID --confirm
./rowd scan
./rowd device test
./rowd device pause
./rowd device resume
```

`share reindex` rebuilds one Share's cache; `scan` requests a broader scan. Removing a Share does not erase its synced files or recovery copies. You can remap an Android folder after choosing how the existing PC and Android files should be compared:

```bash
./rowd share remap SHARE_ID --policy compare
```

The other policies are `pc` and `android`. A remap increments the binding revision and requires you to select the Android folder again through the file picker. Rowd rejects overlapping Share roots, including roots reached through symlinks on Linux.

## Ignore rules

Put `.rowdignore` in a PC Share root. The PC sends its effective ignore policy to Android. The Android copy of `.rowdignore` does not define a second policy.

```text
# Local build output
target/
node_modules/
*.tmp
```

A pattern without `/` matches names at any depth; a pattern with `/` is relative to the Share root. `*` is the only wildcard. There is no negation or full `.gitignore` syntax. Rowd excludes `.rowd/` and `.rowdignore` from transfers.

You can also apply a file of ignore rules through the CLI with `./rowd share ignore SHARE_ID --file /path/to/rules.txt`.

## Limits and recovery

Rowd accepts files up to 8 GiB and up to 50,000 files per Share. It does not sync symlinks, empty folders, original permissions, or timestamps. It transfers complete files and cannot resume a partial file transfer. A trusted CREATE or MODIFY event can use a small delta; DELETE, rename, restart, binding changes, watcher overflow, or provider inconsistency can require a full scan.

The desktop installer uses conditional writes and keeps recovery copies around an interrupted replacement. Android's Storage Access Framework cannot provide the same atomic replace guarantee for every provider. If an Android write has an ambiguous result, Rowd pauses that Share until you resolve its recovery record. Use **Revisar versões recuperáveis** to keep or restore a version, and **Exportar cópias de recuperação** before clearing app data.

The desktop terminal app has a **Recovery** tab. The CLI can list records and act on one record:

```bash
./rowd recovery
./rowd recovery --share SHARE_ID --id RECORD_ID --action restore
./rowd recovery --share SHARE_ID --id RECORD_ID --action export --output /tmp/recovered-file
```

Restoring a version preserves the displaced version. Exporting does not overwrite an existing destination.

The project remains experimental. The Rust tests and Android build run in CI, but they do not cover every physical Android provider, long background sessions, or all interruption points on a real phone. Start with folders you can replace, and keep an independent copy of important files.

## Configuration and backups

Rowd stores PC configuration under `~/.local/share/rowd/.rowd/` by default. Use `--home DIRECTORY` or `ROWD_HOME` to choose another location. This directory contains the pairing credential, certificate, private key, Share state, and recovery data. Keep it private.

You can export a profile without credentials, or an encrypted backup with them:

```bash
./rowd config export-profile /tmp/rowd-profile.json
./rowd config export-backup /tmp/rowd-backup.json --passphrase 'a long passphrase'
./rowd config import-backup /tmp/rowd-backup.json --passphrase 'a long passphrase'
./rowd diagnostic --output /tmp/rowd-diagnostic.json
```

Complete backups use PBKDF2-HMAC-SHA256 and AES-256-GCM. The diagnostic report omits secrets and the private key. If a phone is lost, `./rowd device revoke --confirm` revokes its identity without waiting for an Android acknowledgement. Use `./rowd device unlink --confirm` for a coordinated unlink.

For a reset, run `./rowd reset --level initial --confirm`. The available levels are `share`, `unlink`, `initial`, and `all`; read the command's help before choosing one.

## Upgrade an older installation

Stop old processes and update both devices. Protocol V8 rejects older network peers. Rowd can migrate V1 data from a folder root:

```bash
./rowd migrate --folder /path/to/old-folder --address 192.168.1.20:43821
```

The migration preserves credentials, Android identity, the former folder's Share ID, and its convergence base. Shares whose Android destination was inferred from a subfolder remain paused until you choose an explicit folder on the phone. Keep the old data and its backups until you have checked the migrated Shares.

## Build from source

The repository has four Rust crates and an Android app. Build and check the Linux binary with:

```bash
cargo build --release -p rowd
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked
```

Android requires JDK 17, SDK 35, NDK 27.2, and Gradle 8.9. `scripts/build-android.sh` builds the ARM64 Rust library and a debug APK using the local `.toolchain/` directory. GitHub Actions runs the same Rust and Android compilation paths on pushes and pull requests.
