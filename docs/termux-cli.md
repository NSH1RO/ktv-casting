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

The diagnostics mode also runs read-only SOAP queries and writes a timestamped
`soap-*/` subdirectory containing request/response XML, HTTP status and SOAP
faults in `results.json`, and `summary.txt`. It samples transport state and
position five times, two seconds apart, and queries protocol/media capabilities,
volume, mute, loudness, dB range and presets. ConnectionManager results are used
to discover additional AVTransport/RenderingControl instance IDs (bounded to
three connections). Audio probes include Master and advertised LF/RF channels.
Unsupported/missing fields remain distinguishable from a real zero value.
These queries never issue Play, Pause, Seek, SetVolume or SetMute. To investigate
progress, start playback using your usual controller before entering diagnostics.

To cross-compile with Rust, an Android NDK and cargo-ndk installed:

```sh
rustup target add aarch64-linux-android
CARGO_NDK_PLATFORM=24 cargo ndk -t arm64-v8a build --release --locked --bin ktv-cli --features termux
```

The `termux` feature enables the interactive CLI on Android. Android JNI library
builds without this feature retain their existing entry points.
