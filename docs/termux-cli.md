# Termux CLI (Android ARM64, Android 7.0+)

Download the `ktv-cli-termux-arm64` artifact from the **Personal Windows and
Termux CLI** workflow in `NSH1RO/ktv-casting`, then extract its ZIP to obtain
`ktv-cli-termux-arm64.tar.gz`.

In Termux, copy the archive into your home directory and run:

```sh
cd ~
tar -xzf ktv-cli-termux-arm64.tar.gz
chmod +x ktv-cli
./ktv-cli
```

Run the executable from Termux's home directory, since Android shared storage
(such as `/sdcard/Download`) does not support executing these files. Keep Termux
in the foreground while casting. The phone and renderer must share a network;
if SSDP discovery fails, enter the device's full description XML URL.

For UPnP diagnostics, enter a room URL (used only for the initial input check),
select **1. DLNA**, then **2. Device read-only diagnostics**. This saves
`description.xml`, `service-*.xml`, and `capabilities.txt` under
`dlna-debug/<device-name>/` in the current directory. No room server connection
is needed for this diagnostics path. Media cache files are also stored under
the current directory (`ktv-media-cache/`).

To cross-compile with Rust, an Android NDK and cargo-ndk installed:

```sh
rustup target add aarch64-linux-android
cargo ndk -t arm64-v8a -p 24 build --release --locked --bin ktv-cli --features termux
```

The `termux` feature enables the interactive CLI on Android. Android JNI library
builds without this feature retain their existing entry points.
