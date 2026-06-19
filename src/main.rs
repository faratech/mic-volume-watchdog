#![windows_subsystem = "windows"]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use windows::{
    Win32::Devices::FunctionDiscovery::*,
    Win32::Media::Audio::Endpoints::*,
    Win32::Media::Audio::*,
    Win32::System::Com::StructuredStorage::*,
    Win32::System::Com::*,
    Win32::UI::Shell::PropertiesSystem::*,
};

const NOTIFY_COOLDOWN_SECS: u64 = 30;
const VOLUME_RESTORE_THRESHOLD: f32 = 1.0;
const RETRY_DELAY_MIN_MS: u64 = 500;
const RETRY_DELAY_MAX_MS: u64 = 5_000;

fn format_duration(total_secs: u64) -> String {
    if total_secs == 0 {
        return "0s".into();
    }
    let years = total_secs / (365 * 86400);
    let rem = total_secs % (365 * 86400);
    let weeks = rem / (7 * 86400);
    let rem = rem % (7 * 86400);
    let days = rem / 86400;
    let rem = rem % 86400;
    let hours = rem / 3600;
    let rem = rem % 3600;
    let mins = rem / 60;
    let secs = rem % 60;

    let mut parts = Vec::new();
    if years > 0 { parts.push(format!("{years}y")); }
    if weeks > 0 { parts.push(format!("{weeks}w")); }
    if days > 0 { parts.push(format!("{days}d")); }
    if hours > 0 { parts.push(format!("{hours}h")); }
    if mins > 0 { parts.push(format!("{mins}m")); }
    if secs > 0 { parts.push(format!("{secs}s")); }
    parts.join(" ")
}

fn set_status_error(status: &Arc<Mutex<Status>>, message: Option<String>) {
    if let Ok(mut s) = status.lock() {
        s.error = message;
    }
}

fn volume_needs_fix(level: f32) -> bool {
    level < VOLUME_RESTORE_THRESHOLD
}

fn next_retry_delay(current_ms: u64) -> u64 {
    let doubled = current_ms.saturating_mul(2);
    doubled.min(RETRY_DELAY_MAX_MS)
}

struct Status {
    device_name: String,
    last_volume_fix: Option<Instant>,
    last_unmute: Option<Instant>,
    fixes_count: u32,
    unmutes_count: u32,
    error: Option<String>,
}

fn create_icon(active: bool) -> Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    let (r, g, b) = if active {
        (50u8, 180, 50) // green
    } else {
        (140, 140, 140) // grey
    };

    let set = |rgba: &mut [u8], x: u32, y: u32, r: u8, g: u8, b: u8, a: u8| {
        if x < size && y < size {
            let i = ((y * size + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = a;
        }
    };

    // Draw microphone shape at 32x32
    // Mic head (rounded rect from y=3 to y=16, x=11 to x=20)
    for y in 3..=16 {
        for x in 11..=20 {
            // Round the top corners
            let draw = if y <= 5 {
                let dx = if x < 13 { 13.0 - x as f32 } else if x > 18 { x as f32 - 18.0 } else { 0.0 };
                let dy = 5.0 - y as f32;
                dx * dx + dy * dy <= 6.5
            } else {
                true
            };
            if draw {
                set(&mut rgba, x, y, r, g, b, 255);
            }
        }
    }

    // Mic body rounded bottom (y=16 to y=19)
    for y in 16..=19 {
        let radius = 5.5 - (y as f32 - 16.0) * 0.8;
        let cx = 15.5_f32;
        for x in 10..=21 {
            let dx = (x as f32 - cx).abs();
            if dx <= radius {
                set(&mut rgba, x, y, r, g, b, 255);
            }
        }
    }

    // Arc/cradle around mic (y=14 to y=21)
    let cx = 15.5_f32;
    let cy = 14.0_f32;
    for y in 14..=22 {
        for x in 7..=24 {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if (8.0..=10.0).contains(&dist) && y >= 14 {
                // Only draw the bottom half of the arc
                set(&mut rgba, x, y, r, g, b, 255);
            }
        }
    }

    // Stem (vertical line from bottom of arc, y=22 to y=26)
    for y in 22..=26 {
        for x in 15..=16 {
            set(&mut rgba, x, y, r, g, b, 255);
        }
    }

    // Base (horizontal line at y=27)
    for x in 12..=19 {
        set(&mut rgba, x, 27, r, g, b, 255);
        set(&mut rgba, x, 28, r, g, b, 255);
    }

    // If paused, draw a diagonal red slash
    if !active {
        for i in 2..30 {
            let x = i;
            let y = i;
            if x < size && y < size {
                set(&mut rgba, x, y, 220, 40, 40, 255);
                if x + 1 < size {
                    set(&mut rgba, x + 1, y, 220, 40, 40, 255);
                }
            }
        }
    }

    Icon::from_rgba(rgba, size, size).expect("failed to create icon")
}

fn ensure_single_instance() -> *mut std::ffi::c_void {
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let name: Vec<u16> = "Global\\MicVolumeFix_SingleInstance\0"
            .encode_utf16()
            .collect();
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            std::process::exit(1);
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            std::process::exit(0);
        }
        handle
    }
}

/// Enable Windows Efficiency Mode (EcoQoS) for the current process.
/// Sets idle priority and enables power throttling to minimize battery impact.
fn enable_efficiency_mode() {
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let handle = GetCurrentProcess();

        // Set to idle priority class (lowest scheduling priority)
        let _ = SetPriorityClass(handle, IDLE_PRIORITY_CLASS);

        // Enable EcoQoS power throttling
        let mut throttle_state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        };

        let _ = SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            &mut throttle_state as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn get_exe_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn is_in_startup() -> bool {
    use windows_sys::Win32::System::Registry::*;

    unsafe {
        let subkey: Vec<u16> =
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run\0"
                .encode_utf16()
                .collect();
        let value_name: Vec<u16> = "MicVolumeFix\0".encode_utf16().collect();
        let mut hkey = std::ptr::null_mut();

        let result = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey);
        if result != 0 {
            return false;
        }

        let exists = RegQueryValueExW(
            hkey,
            value_name.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0;

        RegCloseKey(hkey);
        exists
    }
}

fn set_startup(enable: bool) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::*;

    unsafe {
        let subkey: Vec<u16> =
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run\0"
                .encode_utf16()
                .collect();
        let value_name: Vec<u16> = "MicVolumeFix\0".encode_utf16().collect();
        let mut hkey = std::ptr::null_mut();

        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result != 0 {
            return Err(format!("Failed to open startup key: 0x{:08X}", result));
        }

        let status = if enable {
            let exe = get_exe_path().ok_or_else(|| "Failed to resolve executable path".to_string())?;
            let value: Vec<u16> = format!("\"{exe}\"\0").encode_utf16().collect();
            RegSetValueExW(
                hkey,
                value_name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(hkey, value_name.as_ptr())
        };

        RegCloseKey(hkey);

        if status == 0 {
            Ok(())
        } else if enable {
            Err(format!("Failed to enable startup entry: 0x{:08X}", status))
        } else {
            Err(format!("Failed to disable startup entry: 0x{:08X}", status))
        }
    }
}

/// Get the device ID of the current default capture device.
fn get_default_mic_id(enumerator: &IMMDeviceEnumerator) -> Option<String> {
    unsafe {
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?;
        device.GetId().ok()?.to_string().ok()
    }
}

/// Try to get the default capture device and its volume control.
/// Returns (device_id, device_name, IAudioEndpointVolume) or None if no mic.
fn acquire_mic(
    enumerator: &IMMDeviceEnumerator,
) -> Option<(String, String, IAudioEndpointVolume)> {
    unsafe {
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?;

        let device_id = device.GetId().ok()?.to_string().ok()?;

        let props: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
        let name = props.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name_str = PropVariantToStringAlloc(&name)
            .and_then(|s| s.to_string().map_err(|e| e.into()))
            .unwrap_or_else(|_| "Unknown".into());

        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        Some((device_id, name_str, volume))
    }
}

fn main() {
    let _mutex = ensure_single_instance();
    enable_efficiency_mode();

    let paused = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(Status {
        device_name: String::new(),
        last_volume_fix: None,
        last_unmute: None,
        fixes_count: 0,
        unmutes_count: 0,
        error: None,
    }));

    let (notify_tx, notify_rx) = mpsc::channel::<String>();
    let running = Arc::new(AtomicBool::new(true));

    let status_clone = status.clone();
    let paused_clone = paused.clone();
    let running_clone = running.clone();

    let monitor_handle = thread::spawn(move || {
        unsafe {
            let mut com_inited = false;
            let mut enumerator: Option<IMMDeviceEnumerator> = None;
            let mut notify_delay_ms = RETRY_DELAY_MIN_MS;
            let mut enumerator_delay_ms = RETRY_DELAY_MIN_MS;

            let mut mic: Option<(String, String, IAudioEndpointVolume)> = None;
            let mut last_notify = Instant::now() - Duration::from_secs(NOTIFY_COOLDOWN_SECS);
            let mut iteration: u32 = 0;

            loop {
                if !running_clone.load(Ordering::Acquire) {
                    break;
                }

                // If paused, just sleep
                if paused_clone.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                if !com_inited {
                    match CoInitializeEx(None, COINIT_MULTITHREADED) {
                        Ok(()) => {
                            com_inited = true;
                        }
                        Err(err) => {
                            set_status_error(
                                &status_clone,
                                Some(format!("COM initialization failed: {err}")),
                            );
                            thread::sleep(Duration::from_millis(notify_delay_ms));
                            notify_delay_ms = next_retry_delay(notify_delay_ms);
                            continue;
                        }
                    }
                }

                if enumerator.is_none() {
                    match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                        Ok(device_enumerator) => {
                            enumerator = Some(device_enumerator);
                            enumerator_delay_ms = RETRY_DELAY_MIN_MS;
                        }
                        Err(err) => {
                            set_status_error(
                                &status_clone,
                                Some(format!("Failed to create device enumerator: {err}")),
                            );
                            thread::sleep(Duration::from_millis(enumerator_delay_ms));
                            enumerator_delay_ms = next_retry_delay(enumerator_delay_ms);
                            continue;
                        }
                    }
                }

                let enumerator = enumerator.as_ref().expect("enumerator missing");

                // Try to acquire mic if we don't have one
                if mic.is_none() {
                    match acquire_mic(enumerator) {
                        Some((id, name, vol)) => {
                            let mut s = status_clone.lock().unwrap();
                            s.device_name = name.clone();
                            s.error = None;
                            mic = Some((id, name, vol));
                            notify_delay_ms = RETRY_DELAY_MIN_MS;
                            enumerator_delay_ms = RETRY_DELAY_MIN_MS;
                        }
                        None => {
                            let mut s = status_clone.lock().unwrap();
                            s.device_name.clear();
                            s.error = Some("No microphone detected".into());
                            thread::sleep(Duration::from_secs(5));
                            continue;
                        }
                    }
                }

                // Periodically check if the default device has changed
                iteration = iteration.wrapping_add(1);
                if iteration % 10 == 0 {
                    if let Some(new_id) = get_default_mic_id(&enumerator) {
                        let current_id = &mic.as_ref().unwrap().0;
                        if &new_id != current_id {
                            // Default device changed — re-acquire
                            {
                                let mut s = status_clone.lock().unwrap();
                                s.error = Some("Default microphone changed, reconnecting...".into());
                            }
                            mic = None;
                            notify_delay_ms = RETRY_DELAY_MIN_MS;
                            enumerator_delay_ms = RETRY_DELAY_MIN_MS;
                            continue;
                        }
                    } else {
                        let mut s = status_clone.lock().unwrap();
                        s.error = Some("Failed to read default microphone ID".into());
                        mic = None;
                        thread::sleep(Duration::from_millis(notify_delay_ms));
                        notify_delay_ms = next_retry_delay(notify_delay_ms);
                        continue;
                    }
                }

                let (_id, _name, volume) = mic.as_ref().unwrap();

                let mut did_unmute = false;
                let mut did_fix_volume = false;
                let mut old_pct = 100u32;
                let mut had_recoverable_error = false;

                // Check mute — if the call fails, the device is gone
                match volume.GetMute() {
                    Ok(muted) if muted.as_bool() => {
                        if volume.SetMute(false, std::ptr::null()).is_ok() {
                            let mut s = status_clone.lock().unwrap();
                            s.last_unmute = Some(Instant::now());
                            s.unmutes_count += 1;
                            did_unmute = true;
                        } else {
                            had_recoverable_error = true;
                            set_status_error(
                                &status_clone,
                                Some("Failed to unmute microphone".into()),
                            );
                        }
                    }
                    Err(_) => {
                        // Device lost — drop and re-acquire next loop
                        mic = None;
                        {
                            let mut s = status_clone.lock().unwrap();
                            s.error = Some("Microphone disconnected, reconnecting...".into());
                        }
                        thread::sleep(Duration::from_millis(notify_delay_ms));
                        notify_delay_ms = next_retry_delay(notify_delay_ms);
                        continue;
                    }
                    _ => {}
                }

                match volume.GetMasterVolumeLevelScalar() {
                    Ok(current) => {
                        let normalized = current.clamp(0.0, 1.0);
                        old_pct = (normalized * 100.0).round() as u32;
                        if volume_needs_fix(normalized) {
                            if volume
                                .SetMasterVolumeLevelScalar(1.0, std::ptr::null())
                                .is_ok()
                            {
                                let mut s = status_clone.lock().unwrap();
                                s.last_volume_fix = Some(Instant::now());
                                s.fixes_count += 1;
                                did_fix_volume = true;
                            } else {
                                had_recoverable_error = true;
                                set_status_error(
                                    &status_clone,
                                    Some("Failed to restore microphone volume".into()),
                                );
                            }
                        }
                    }
                    Err(_) => {
                        // Device lost — drop and re-acquire next loop
                        mic = None;
                        {
                            let mut s = status_clone.lock().unwrap();
                            s.error = Some("Microphone disconnected, reconnecting...".into());
                        }
                        thread::sleep(Duration::from_millis(notify_delay_ms));
                        notify_delay_ms = next_retry_delay(notify_delay_ms);
                        continue;
                    }
                }

                if had_recoverable_error {
                    thread::sleep(Duration::from_millis(notify_delay_ms));
                    notify_delay_ms = next_retry_delay(notify_delay_ms);
                    continue;
                }

                set_status_error(&status_clone, None);
                notify_delay_ms = RETRY_DELAY_MIN_MS;

                // Send notification with cooldown
                if (did_unmute || did_fix_volume)
                    && last_notify.elapsed().as_secs() >= NOTIFY_COOLDOWN_SECS
                {
                    let msg = if did_unmute && did_fix_volume {
                        format!("Unmuted and volume raised from {old_pct}% to 100%")
                    } else if did_unmute {
                        "Microphone was muted - unmuted".into()
                    } else {
                        format!("Volume raised from {old_pct}% to 100%")
                    };
                    let _ = notify_tx.send(msg);
                    last_notify = Instant::now();
                }

                thread::sleep(Duration::from_secs(1));
            }
            if com_inited {
                CoUninitialize();
            }
        }
    });

    // Build tray menu
    let menu = Menu::new();
    let device_item = MenuItem::new("Mic: starting...", false, None);
    let volume_item = MenuItem::new("Volume: checking...", false, None);
    let unmute_item = MenuItem::new("Unmute: n/a", false, None);
    let pause_item = CheckMenuItem::new("Paused", true, false, None);
    let startup_item = CheckMenuItem::new("Start with Windows", true, is_in_startup(), None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&device_item).unwrap();
    menu.append(&volume_item).unwrap();
    menu.append(&unmute_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&pause_item).unwrap();
    menu.append(&startup_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit_item).unwrap();

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Mic Volume Fix")
        .with_icon(create_icon(true))
        .build()
        .expect("Failed to create tray icon");

    let menu_receiver = MenuEvent::receiver().clone();
    let mut last_menu_update = Instant::now() - Duration::from_secs(1);
    let mut last_device_text = String::new();
    let mut last_volume_text = String::new();
    let mut last_unmute_text = String::new();
    let mut should_quit = false;

    'main: loop {
        while let Ok(event) = menu_receiver.try_recv() {
            if event.id() == quit_item.id() {
                running.store(false, Ordering::Release);
                should_quit = true;
                break;
            } else if event.id() == pause_item.id() {
                let is_paused = pause_item.is_checked();
                paused.store(is_paused, Ordering::Relaxed);
                let _ = _tray.set_icon(Some(create_icon(!is_paused)));
                let _ = _tray.set_tooltip(if is_paused {
                    Some("Mic Volume Fix (paused)")
                } else {
                    Some("Mic Volume Fix")
                });
            } else if event.id() == startup_item.id() {
                let is_enabled = startup_item.is_checked();
                match set_startup(is_enabled) {
                    Ok(()) => set_status_error(&status, None),
                    Err(err) => {
                        startup_item.set_checked(!is_enabled);
                        set_status_error(&status, Some(format!("Startup update failed: {err}")));
                    }
                }
            }
        }

        // Show Windows toast notifications
        while let Ok(msg) = notify_rx.try_recv() {
            use tauri_winrt_notification::{Sound, Toast};
            let _ = Toast::new("windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel")
                .title("Mic Volume Fix")
                .text1(&msg)
                .sound(Some(Sound::Default))
                .duration(tauri_winrt_notification::Duration::Short)
                .show();
        }

        // Update menu items (once per second, not every 100ms)
        if last_menu_update.elapsed().as_secs() >= 1 {
            last_menu_update = Instant::now();
            if let Ok(s) = status.lock() {
                let device_text = if let Some(ref err) = s.error {
                    format!("Mic: {err}")
                } else if s.device_name.is_empty() {
                    "Mic: no device".to_string()
                } else {
                    format!("Mic: {}", s.device_name)
                };
                if device_text != last_device_text {
                    let _ = device_item.set_text(device_text.clone());
                    last_device_text = device_text;
                }

                let volume_text = if let Some(t) = s.last_volume_fix {
                    let ago = format_duration(t.elapsed().as_secs());
                    format!(
                        "Volume fixed: {ago} ago (total: {})",
                        s.fixes_count
                    )
                } else {
                    "Volume: no fixes needed yet".to_string()
                };
                if volume_text != last_volume_text {
                    let _ = volume_item.set_text(volume_text.clone());
                    last_volume_text = volume_text;
                }

                let unmute_text = if let Some(t) = s.last_unmute {
                    let ago = format_duration(t.elapsed().as_secs());
                    format!(
                        "Unmuted: {ago} ago (total: {})",
                        s.unmutes_count
                    )
                } else {
                    "Unmute: no fixes needed yet".to_string()
                };
                if unmute_text != last_unmute_text {
                    let _ = unmute_item.set_text(unmute_text.clone());
                    last_unmute_text = unmute_text;
                }
            }
        }

        if should_quit {
            break 'main;
        }

        // Pump Win32 messages
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    break 'main;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        thread::sleep(Duration::from_millis(100));
    }

    running.store(false, Ordering::Release);
    if let Err(_join_err) = monitor_handle.join() {
        let _ = _join_err;
    }
}
