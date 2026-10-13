# Mic Volume Watchdog

[![Platform: Windows](https://img.shields.io/badge/Platform-Windows%2010%2B%20(x64%20%7C%20ARM64)-blue.svg)](https://microsoft.com/windows)
[![Language: Rust](https://img.shields.io/badge/Language-Rust%202024-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

**Mic Volume Watchdog** is an ultra-lightweight, high-performance Windows system tray utility written in Rust. It continuously monitors your default recording device and persistently enforces **100% volume** and an **unmuted state**.

Built to prevent VoIP applications (such as Paltalk, Zoom, Microsoft Teams, Discord, Skype, and WebRTC browsers) and Windows communication ducking from silently lowering your microphone input gain or muting your audio without your consent.

> [!NOTE]
> This repository consolidates and supersedes the legacy [`mic-volume-fix`](https://github.com/faratech/mic-volume-fix) prototype, replacing high-frequency polling with native event-driven Core Audio callbacks, Windows EcoQoS (Efficiency Mode), and near-zero idle CPU wakeups.

---

## The Problem & The Solution

* **The Problem**: Many VoIP and communication apps employ aggressive Automatic Gain Control (AGC) or trigger Windows communication activity detection that automatically lowers microphone sensitivity. Users frequently get complaints like *"We can't hear you!"* without ever realizing their microphone volume was turned down.
* **The Solution**: Rather than polling the audio system constantly or requiring manual intervention, Mic Volume Watchdog hooks directly into Windows Core Audio via native COM callbacks (`IAudioEndpointVolumeCallback`). The instant any external process lowers the gain or toggles mute, the watchdog catches the event, corrects the endpoint within milliseconds, verifies the change, and sends an optional toast notification.

---

## Key Features

* **Event-Driven Detection**: Uses native Windows Core Audio `IAudioEndpointVolumeCallback` notifications. Corrections occur within a debounced **~40ms** window rather than waiting on a multi-second polling timer.
* **Ultra-Low Resource Footprint**:
  * **Near-Zero Idle Wakeups**: Employs `MsgWaitForMultipleObjects` to sleep until Win32 messages or a 1-second telemetry interval occurs (~1 wakeup/sec while idle, down from 10+ wakeups/sec in legacy polling utilities).
  * **Windows EcoQoS / Efficiency Mode**: Automatically requests `ProcessPowerThrottling` EcoQoS from Windows to reduce CPU priority and maximize laptop battery longevity.
* **Self-Healing & Auto-Recovery**:
  * **Write Verification & Retries**: Verifies audio endpoint writes and performs automated retries on transient COM errors.
  * **Device Rebinding**: Automatically tracks default recording device changes and re-acquires COM endpoints upon hardware plug, unplug, or Bluetooth reconnection.
  * **Periodic Safety Check**: Runs an auxiliary 1-second safety audit (250ms fallback if callback registration fails) to guarantee state persistence.
* **System Tray Interface & Live Telemetry**:
  * Programmatically rendered tray icon: **Green microphone** when active, **Gray with red slash** when paused.
  * Live metrics displayed directly in the context menu:
    * Active capture device name
    * Total volume corrections & time since last fix
    * Total unmute corrections & time since last unmute
    * Granular diagnostic counters (restore attempts, misses, reconnects, write retries, callback health)
  * One-click **Pause / Resume** toggle.
  * One-click **Start with Windows** toggle (persists via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).
* **Desktop Notifications**: Displays native Windows Action Center toast notifications upon correction (throttled by an automated 30-second cooldown to prevent notification fatigue).
* **Multi-User Safe**: Employs a session-scoped named mutex (`Local\MicVolumeFix_SingleInstance`) allowing Windows Fast User Switching across multiple active user accounts.
* **Tiny Standalone Binary**: Zero runtime dependencies, no installers required (~250 KB standalone executable).

---

## Installation & Usage

1. Download the executable corresponding to your processor architecture:
   * **`mic_volume_fix_x64.exe`**: Intel & AMD 64-bit systems.
   * **`mic_volume_fix_arm64.exe`**: ARM64 systems (Surface Pro, Snapdragon X Elite, Windows on ARM).
2. Run the executable. It starts silently in the system tray / notification area with a green microphone icon.
3. **Tray Menu**:
   * Right-click the tray icon to review current device status and telemetry.
   * Check **Pause** to temporarily allow third-party apps to adjust volume.
   * Check **Start with Windows** to run automatically at user login.
   * Click **Quit** to gracefully shut down the watchdog.

---

## System Requirements

* **Operating System**: Windows 10 (version 1809+) or Windows 11 (x64 or ARM64).
* **Audio Hardware**: Any Core Audio compatible recording device (built-in microphone, USB headset, audio interface, or Bluetooth microphone).

---

## Building from Source

### Prerequisites

* [Rust](https://rustup.rs/) (2024 edition or newer)
* Windows SDK (if compiling on Windows) or cross-compilation toolchains (if compiling on Linux)

### Native Windows Build

```sh
# Build optimized release binary
cargo build --release

# Output binary located at:
# target/release/mic_volume_fix.exe
```

### Cross-Compilation from Linux

Using `cargo-xwin` (recommended for MSVC ABI output):

```sh
# Install cargo-xwin and Windows SDK cache
cargo install cargo-xwin

# Run the automated build script (builds both x64 and ARM64):
./build.sh [optional_output_directory]
```

Using standard MinGW GNU targets:

```sh
# For x86_64
cargo build --release --target x86_64-pc-windows-gnu

# For ARM64
cargo build --release --target aarch64-pc-windows-gnullvm
```

---

## Repository Structure

```text
├── .github/workflows/         # Shared CI policies and lint workflows
├── src/
│   └── main.rs                # Core watchdog engine, COM callbacks, Win32 message loop & tray UI
├── third_party/
│   ├── embed-resource/        # Vendored resource embedding crate for icons & manifest
│   └── tauri-winrt-notification/ # Toast notification integration
├── tools/
│   ├── cargo-auto-bump/       # SemVer version bump utility
│   ├── host_mic_optimization_test.ps1 # Automated burst-test & latency validation script
│   ├── run-auto-bump.sh       # Auto-bump helper runner
│   └── run-host-mic-opt-test.sh # Host integration test runner
├── app.ico                    # Embedded application icon
├── app.rc                     # Windows resource script
├── build.rs                   # Build script embedding icon resources
├── build.sh                   # Multi-target release build script
├── Cargo.toml                 # Cargo dependencies & size-optimization profile
└── README.md                  # Project documentation
```

---

## Quality & Optimization Flags

The release profile in `Cargo.toml` is optimized for minimal binary size, zero console allocation, and maximum responsiveness:

```toml
[profile.release]
opt-level = "z"        # Optimize for size
lto = true             # Link-time optimization (cross-crate dead code elimination)
codegen-units = 1      # Single codegen unit for optimal compiler reasoning
strip = "symbols"      # Strip debug symbols
panic = "abort"        # Eliminate unwinding overhead
```

---

## License

This project is licensed under the [MIT License](LICENSE).
