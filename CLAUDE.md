# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Windows system tray utility (Rust) that monitors the default microphone and restores it to 100% volume whenever something lowers it or mutes it. Built for Paltalk users who experience involuntary mic volume changes. Single-file application (`src/main.rs`, ~1300 lines).

## Build Commands

```bash
# Native build (for testing compilation on Linux)
cargo build --release

# Cross-compile for Windows x64 (primary target)
cargo build --release --target x86_64-pc-windows-gnu

# Cross-compile for Windows ARM64
cargo build --release --target aarch64-pc-windows-gnullvm

# Release builds (both shipping exes, MSVC toolchain via cargo-xwin)
./build.sh [output_dir]
```

No tests or linter configured. The project has no test suite.

## Architecture

Everything lives in `src/main.rs` — a single-threaded tray app with one background monitoring thread:

- **Main thread**: Win32 message pump + tray icon (via `tray-icon` crate). Blocks in `MsgWaitForMultipleObjects` (1s timeout) so tray/menu input reacts immediately with ~1 idle wake/sec. Handles menu events (pause/resume, startup toggle, quit), updates menu item text with live stats, and displays Windows toast notifications (via `tauri-winrt-notification`).
- **Monitor thread**: Initializes COM (`COINIT_MULTITHREADED`), acquires the default capture device through `IMMDeviceEnumerator`, and registers an `IAudioEndpointVolumeCallback` for event-driven detection (external changes are debounced 40ms, anchored to the first event of a burst). A periodic safety check runs every 1s (250ms if callback registration failed). If volume < 100% or mic is muted, it corrects immediately, verifies the write, and retries once on failure. Handles device disconnection and default-device changes by re-acquiring. Timestamps are `Option<Instant>` — never subtract a `Duration` from `Instant::now()`, it panics when the process starts sooner after boot than the offset.
- **Communication**: `Arc<AtomicBool>` for pause state, `Arc<Mutex<Status>>` for stats, `mpsc::channel` for notification messages from monitor to main thread.

Key platform details:
- `#![windows_subsystem = "windows"]` — no console window
- Single-instance enforcement via a named Windows mutex (`Local\MicVolumeFix_SingleInstance`, per-session so fast user switching works)
- Start-with-Windows via `HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Run` registry key
- Tray icon is drawn programmatically in `create_icon()` (32x32 RGBA) — green mic when active, grey mic with red slash when paused
- `build.rs` embeds `app.ico` as the exe icon via `embed-resource`
- Release profile is tuned for minimal binary size (`opt-level = "z"`, LTO, strip, abort on panic)
