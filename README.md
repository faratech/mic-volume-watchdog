# Mic Volume Watchdog

A lightweight, high-performance Windows system tray utility in Rust that monitors the default recording endpoint and persistently enforces 100% volume and unmuted state. Built to combat aggressive VoIP applications (such as Paltalk, Zoom, Teams) or Windows communication ducking that silently lowers or mutes microphone input.

> [!NOTE]
> This repository consolidates and supersedes the legacy `mic-volume-fix` utility, replacing periodic polling with native event-driven Core Audio callbacks, EcoQoS/Efficiency Mode, and minimal idle CPU wakeups.

## Key Features

- **Event-Driven Detection**: Uses native Windows Core Audio `IAudioEndpointVolumeCallback` notifications to react immediately (< 40ms debounce) when any external application alters volume or mute states.
- **Ultra-Low Resource Footprint**:
  - Replaces busy-wait polling with `MsgWaitForMultipleObjects` (~1 idle wakeup/sec).
  - Configured with Windows EcoQoS (Efficiency Mode) to prioritize battery life and minimize CPU utilization.
- **Robustness & Auto-Recovery**:
  - Verifies audio endpoint writes and retries on transient COM errors.
  - Automatically recovers and rebinds upon audio device disconnection, reconnection, or default device switching.
  - Session-scoped single-instance enforcement (`Local\` mutex namespace) compatible with Fast User Switching.
- **System Tray UI & Real-Time Telemetry**:
  - Visual status indicator (green mic = active, grey mic with slash = paused).
  - Tray menu tracks real-time statistics: active capture device, correction counts, restore attempts, and endpoint write diagnostics.
  - Optional Windows toast notifications upon corrections.
  - Configurable start-with-Windows registry integration.

## Build Instructions

Cross-compile or build natively with Cargo:

```sh
# Cross-compile for Windows x64 (primary)
cargo build --target x86_64-pc-windows-gnu --release --locked

# Cross-compile for Windows ARM64
cargo build --target aarch64-pc-windows-gnullvm --release --locked

# Release builds (both shipping exes, MSVC toolchain via cargo-xwin)
./build.sh [output_dir]
```

## License

MIT. See [LICENSE](LICENSE).
