#![windows_subsystem = "windows"]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows::core::GUID;
use windows_core::implement;

use windows::{
    Win32::Devices::FunctionDiscovery::*, Win32::Media::Audio::Endpoints::*,
    Win32::Media::Audio::*, Win32::System::Com::StructuredStorage::*, Win32::System::Com::*,
    Win32::UI::Shell::PropertiesSystem::*,
};

const NOTIFY_COOLDOWN_SECS: u64 = 30;
const VOLUME_RESTORE_THRESHOLD: f32 = 1.0;
const RETRY_DELAY_MIN_MS: u64 = 500;
const RETRY_DELAY_MAX_MS: u64 = 5_000;
const EXTERNAL_DEBOUNCE_WINDOW_MS: u64 = 40;
const PERIODIC_CHECK_INTERVAL_MS: u64 = 1_000;
const PERIODIC_CHECK_NO_CALLBACK_INTERVAL_MS: u64 = 250;
const PAUSED_SLEEP_MS: u64 = 250;
const RESTORE_RATE_LIMIT_MS: u64 = 75;
const RESTORE_RATE_LIMIT_MAX_MS: u64 = 250;
const UNSTABLE_EXTERNAL_RETRY_MS: u64 = 10;
const EXTERNAL_READ_RETRY_LIMIT: u32 = 10;
const MAIN_LOOP_WAIT_MS: u32 = 1_000;
const MIC_FIXER_CONTEXT: GUID = GUID::from_u128(0x5e4f84ce_2b8f_4a74_b7f8_1b7f9d4e1c6a);

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    tx: mpsc::SyncSender<()>,
    allowed_context: GUID,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows_core::Result<()> {
        let allow =
            unsafe { !pnotify.is_null() && (*pnotify).guidEventContext == self.allowed_context };

        if !allow {
            let _ = self.tx.try_send(());
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ActionReason {
    Starting,
    AcquiredDefaultMic,
    CallbackRegistrationFailed,
    DefaultMicChanged,
    DefaultMicCheckFailed,
    ExternalChange,
    ExternalStateRetry,
    NoRestoreNeeded,
    PeriodicCheck,
    PeriodicCheckStable,
    Reconnect,
    RestoreRateLimited,
    WriteFailed,
    WriteVerified,
}

impl ActionReason {
    fn label(self) -> &'static str {
        match self {
            ActionReason::Starting => "starting",
            ActionReason::AcquiredDefaultMic => "acquired default mic",
            ActionReason::CallbackRegistrationFailed => "callback registration failed",
            ActionReason::DefaultMicChanged => "default mic changed",
            ActionReason::DefaultMicCheckFailed => "default mic check failed",
            ActionReason::ExternalChange => "external-change",
            ActionReason::ExternalStateRetry => "external-state retry",
            ActionReason::NoRestoreNeeded => "no restore needed",
            ActionReason::PeriodicCheck => "periodic-check",
            ActionReason::PeriodicCheckStable => "periodic check stable",
            ActionReason::Reconnect => "reconnect",
            ActionReason::RestoreRateLimited => "restore rate limited",
            ActionReason::WriteFailed => "write-failed",
            ActionReason::WriteVerified => "write-verified",
        }
    }
}

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
    if years > 0 {
        parts.push(format!("{years}y"));
    }
    if weeks > 0 {
        parts.push(format!("{weeks}w"));
    }
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if secs > 0 {
        parts.push(format!("{secs}s"));
    }
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

/// Sleep up to `total_ms`, waking early if `running` is cleared so quit/pause
/// never has to wait out a saturated retry backoff.
fn interruptible_sleep(running: &AtomicBool, total_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(total_ms);
    while running.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

struct Status {
    device_name: String,
    last_volume_fix: Option<Instant>,
    last_unmute: Option<Instant>,
    fixes_count: u32,
    unmutes_count: u32,
    reconnects: u32,
    callback_registration_failures: u32,
    write_retries: u32,
    write_failures: u32,
    post_write_verify_failures: u32,
    restore_attempts: u32,
    restore_misses: u32,
    restore_noop_cycles: u32,
    error: Option<String>,
    last_action_reason: ActionReason,
}

struct MicHandle {
    id: String,
    name: String,
    volume: IAudioEndpointVolume,
    callback: Option<IAudioEndpointVolumeCallback>,
    callback_registered: bool,
}

impl Drop for MicHandle {
    fn drop(&mut self) {
        if self.callback_registered
            && let Some(callback) = self.callback.take()
        {
            let _ = unsafe { self.volume.UnregisterControlChangeNotify(&callback) };
        }
    }
}

#[derive(Clone, Copy)]
struct MicReading {
    muted: bool,
    normalized_level: f32,
    percent: u32,
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
                let dx = if x < 13 {
                    13.0 - x as f32
                } else if x > 18 {
                    x as f32 - 18.0
                } else {
                    0.0
                };
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
        let name: Vec<u16> = "Local\\MicVolumeFix_SingleInstance\0"
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
/// The monitor is event-driven, so this keeps laptop impact low without
/// relying on a high-frequency idle loop for responsiveness.
fn enable_efficiency_mode() {
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let handle = GetCurrentProcess();

        let _ = SetPriorityClass(handle, IDLE_PRIORITY_CLASS);

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
            &raw mut throttle_state as *mut _,
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
        let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run\0"
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
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::*;

    unsafe {
        let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run\0"
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
            let exe =
                get_exe_path().ok_or_else(|| "Failed to resolve executable path".to_string())?;
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
            let status = RegDeleteValueW(hkey, value_name.as_ptr());
            if status == ERROR_FILE_NOT_FOUND {
                0
            } else {
                status
            }
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

/// Convert a COM-allocated string to an owned String, always freeing the
/// buffer (GetId / PropVariantToStringAlloc allocate via CoTaskMemAlloc and
/// the caller must free).
unsafe fn take_com_string(ptr: windows_core::PWSTR) -> Option<String> {
    let result = unsafe { ptr.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(ptr.0 as *const _)) };
    result
}

/// Get the device ID of the current default capture device.
fn get_default_mic_id(enumerator: &IMMDeviceEnumerator) -> Option<String> {
    unsafe {
        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()?;
        take_com_string(device.GetId().ok()?)
    }
}

fn get_mic_reading(volume: &IAudioEndpointVolume) -> Option<MicReading> {
    unsafe {
        let muted = volume.GetMute().ok()?;
        let level = volume.GetMasterVolumeLevelScalar().ok()?;
        let normalized = level.clamp(0.0, 1.0);
        Some(MicReading {
            muted: muted.as_bool(),
            normalized_level: normalized,
            percent: (normalized * 100.0).round() as u32,
        })
    }
}

fn register_mic_callback(
    mic: &mut MicHandle,
    callback_tx: &mpsc::SyncSender<()>,
    callback_context: GUID,
    status: &Arc<Mutex<Status>>,
) -> Option<IAudioEndpointVolumeCallback> {
    let callback = VolumeCallback {
        tx: callback_tx.clone(),
        allowed_context: callback_context,
    }
    .into();

    if unsafe { mic.volume.RegisterControlChangeNotify(&callback) }.is_ok() {
        Some(callback)
    } else {
        let mut s = status.lock().unwrap();
        s.callback_registration_failures += 1;
        s.last_action_reason = ActionReason::CallbackRegistrationFailed;
        None
    }
}

fn acquire_mic_with_callback(
    enumerator: &IMMDeviceEnumerator,
    callback_tx: &mpsc::SyncSender<()>,
    callback_context: GUID,
    status: &Arc<Mutex<Status>>,
) -> Option<MicHandle> {
    let mut new_mic = acquire_mic(enumerator)?;
    let callback = register_mic_callback(&mut new_mic, callback_tx, callback_context, status);
    new_mic.callback_registered = callback.is_some();
    new_mic.callback = callback;

    let mut s = status.lock().unwrap();
    s.device_name = new_mic.name.clone();
    s.error = None;

    Some(new_mic)
}

fn ensure_default_mic_for_write(
    enumerator: &IMMDeviceEnumerator,
    expected_mic_id: &str,
    force_refresh: bool,
    callback_tx: &mpsc::SyncSender<()>,
    callback_context: GUID,
    status: &Arc<Mutex<Status>>,
    mic: &mut Option<MicHandle>,
) -> Option<IAudioEndpointVolume> {
    let current_default = match get_default_mic_id(enumerator) {
        Some(current_default) => current_default,
        None => {
            {
                let mut s = status.lock().unwrap();
                s.reconnects += 1;
                s.error = Some("Failed to verify default microphone".into());
                s.last_action_reason = ActionReason::Reconnect;
            }
            *mic = None;
            return None;
        }
    };

    if current_default != expected_mic_id {
        {
            let mut s = status.lock().unwrap();
            s.reconnects += 1;
            s.error = Some("Default microphone changed, reconnecting...".into());
            s.last_action_reason = ActionReason::Reconnect;
        }
        *mic = None;
        return None;
    }

    if !force_refresh {
        if let Some(existing) = mic.as_ref() {
            return Some(existing.volume.clone());
        }

        {
            let mut s = status.lock().unwrap();
            s.reconnects += 1;
            s.error = Some("Microphone handle missing, reconnecting...".into());
            s.last_action_reason = ActionReason::Reconnect;
        }
        return None;
    }

    let refreshed =
        match acquire_mic_with_callback(enumerator, callback_tx, callback_context, status) {
            Some(refreshed) => refreshed,
            None => {
                *mic = None;
                return None;
            }
        };

    let volume = refreshed.volume.clone();
    if refreshed.id != expected_mic_id {
        {
            let mut s = status.lock().unwrap();
            s.reconnects += 1;
            s.error = Some("Default microphone changed, reconnecting...".into());
            s.last_action_reason = ActionReason::Reconnect;
        }
        *mic = None;
        return None;
    }

    {
        let mut s = status.lock().unwrap();
        s.reconnects += 1;
        s.last_action_reason = ActionReason::Reconnect;
    }

    *mic = Some(refreshed);
    Some(volume)
}

/// Try to get the default capture device and its volume control.
/// Returns a tracked capture-device handle or None if no mic.
fn acquire_mic(enumerator: &IMMDeviceEnumerator) -> Option<MicHandle> {
    unsafe {
        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()?;

        let device_id = take_com_string(device.GetId().ok()?)?;

        let props: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
        let name = props.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name_str = PropVariantToStringAlloc(&name)
            .ok()
            .and_then(|s| take_com_string(s))
            .unwrap_or_else(|| "Unknown".into());

        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        Some(MicHandle {
            id: device_id,
            name: name_str,
            volume,
            callback: None,
            callback_registered: false,
        })
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
        reconnects: 0,
        callback_registration_failures: 0,
        write_retries: 0,
        write_failures: 0,
        post_write_verify_failures: 0,
        restore_attempts: 0,
        restore_misses: 0,
        restore_noop_cycles: 0,
        error: None,
        last_action_reason: ActionReason::Starting,
    }));

    let (notify_tx, notify_rx) = mpsc::sync_channel::<String>(1);
    let (callback_tx, callback_rx) = mpsc::sync_channel::<()>(1);
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
            let callback_context = MIC_FIXER_CONTEXT;

            // Timestamps use Option<Instant> with None meaning "never / due
            // now": subtracting a Duration from Instant::now() panics when the
            // process starts sooner after boot than the offset (QPC epoch is
            // boot), and this app auto-starts at login.
            let mut mic: Option<MicHandle> = None;
            let mut last_notify: Option<Instant> = None;
            let mut pending_external_change: Option<Instant> = None;
            let mut last_restore_action: Option<Instant> = None;
            let mut restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
            let mut last_periodic_check: Option<Instant> = None;
            let mut last_idle_status_update: Option<Instant> = None;
            let mut external_read_retries: u32 = 0;

            loop {
                if !running_clone.load(Ordering::Acquire) {
                    break;
                }

                // If paused, just sleep
                if paused_clone.load(Ordering::Relaxed) {
                    let _ = callback_rx.recv_timeout(Duration::from_millis(PAUSED_SLEEP_MS));
                    continue;
                }

                if !com_inited {
                    let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                    if hr.is_ok() {
                        com_inited = true;
                    } else {
                        set_status_error(
                            &status_clone,
                            Some(format!("COM initialization failed: {hr:?}")),
                        );
                        interruptible_sleep(&running_clone, notify_delay_ms);
                        notify_delay_ms = next_retry_delay(notify_delay_ms);
                        continue;
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
                            interruptible_sleep(&running_clone, enumerator_delay_ms);
                            enumerator_delay_ms = next_retry_delay(enumerator_delay_ms);
                            continue;
                        }
                    }
                }

                let enumerator = enumerator.as_ref().expect("enumerator missing");

                // Try to acquire mic if we don't have one
                if mic.is_none() {
                    match acquire_mic_with_callback(
                        enumerator,
                        &callback_tx,
                        callback_context,
                        &status_clone,
                    ) {
                        None => {
                            interruptible_sleep(&running_clone, notify_delay_ms);
                            notify_delay_ms = next_retry_delay(notify_delay_ms);
                            continue;
                        }

                        Some(new_mic) => {
                            let mut s = status_clone.lock().unwrap();
                            s.last_action_reason = ActionReason::AcquiredDefaultMic;
                            mic = Some(new_mic);
                            notify_delay_ms = RETRY_DELAY_MIN_MS;
                            enumerator_delay_ms = RETRY_DELAY_MIN_MS;
                        }
                    }
                }

                // The volume callback reports changes in real time, so the
                // periodic pass only needs to be fast when registration failed.
                let periodic_interval =
                    Duration::from_millis(if mic.as_ref().is_some_and(|m| m.callback_registered) {
                        PERIODIC_CHECK_INTERVAL_MS
                    } else {
                        PERIODIC_CHECK_NO_CALLBACK_INTERVAL_MS
                    });

                let now = Instant::now();
                let next_wake = if let Some(change_time) = pending_external_change {
                    change_time + Duration::from_millis(EXTERNAL_DEBOUNCE_WINDOW_MS)
                } else if let Some(checked_at) = last_periodic_check {
                    checked_at + periodic_interval
                } else {
                    now
                };
                let wait_for = next_wake.saturating_duration_since(now);

                // The debounce window is anchored to the FIRST event of a
                // burst; re-anchoring on every event would let sustained
                // sub-40ms volume writes postpone enforcement indefinitely.
                if !wait_for.is_zero() {
                    match callback_rx.recv_timeout(wait_for) {
                        Ok(()) => {
                            if pending_external_change.is_none() {
                                pending_external_change = Some(Instant::now());
                            }
                            while callback_rx.try_recv().is_ok() {}
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    let mut callback_hit = false;
                    while callback_rx.try_recv().is_ok() {
                        callback_hit = true;
                    }
                    if callback_hit && pending_external_change.is_none() {
                        pending_external_change = Some(Instant::now());
                        continue;
                    }
                }

                let now = Instant::now();
                let due_periodic = last_periodic_check
                    .is_none_or(|checked_at| now.duration_since(checked_at) >= periodic_interval);
                let external_change_handling = if let Some(change_time) = pending_external_change {
                    now.duration_since(change_time)
                        >= Duration::from_millis(EXTERNAL_DEBOUNCE_WINDOW_MS)
                } else {
                    false
                };

                if !due_periodic && !external_change_handling {
                    continue;
                }

                let volume = mic.as_ref().expect("mic missing").volume.clone();
                let is_periodic_check = due_periodic && !external_change_handling;
                match get_default_mic_id(enumerator) {
                    Some(current_default_id)
                        if mic.as_ref().is_some_and(|m| m.id == current_default_id) =>
                    {
                        // Still default: proceed.
                        last_periodic_check = Some(now);
                    }
                    Some(_) => {
                        {
                            let mut s = status_clone.lock().unwrap();
                            s.error = Some("Default microphone changed, reconnecting...".into());
                            s.last_action_reason = ActionReason::DefaultMicChanged;
                            s.reconnects += 1;
                        }
                        mic = None;
                        restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                        notify_delay_ms = RETRY_DELAY_MIN_MS;
                        enumerator_delay_ms = RETRY_DELAY_MIN_MS;
                        pending_external_change = None;
                        continue;
                    }
                    None => {
                        {
                            let mut s = status_clone.lock().unwrap();
                            s.error = Some("Failed to verify default microphone".into());
                            s.last_action_reason = ActionReason::DefaultMicCheckFailed;
                            s.reconnects += 1;
                        }
                        mic = None;
                        restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                        interruptible_sleep(&running_clone, notify_delay_ms);
                        notify_delay_ms = next_retry_delay(notify_delay_ms);
                        pending_external_change = None;
                        continue;
                    }
                }

                let Some(reading) = get_mic_reading(&volume) else {
                    if external_change_handling && external_read_retries < EXTERNAL_READ_RETRY_LIMIT
                    {
                        external_read_retries += 1;
                        {
                            let mut s = status_clone.lock().unwrap();
                            s.last_action_reason = ActionReason::ExternalStateRetry;
                        }
                        thread::sleep(Duration::from_millis(UNSTABLE_EXTERNAL_RETRY_MS));
                        continue;
                    }
                    external_read_retries = 0;
                    mic = None;
                    {
                        let mut s = status_clone.lock().unwrap();
                        s.reconnects += 1;
                        s.error = Some("Failed to read microphone state, reconnecting...".into());
                        s.last_action_reason = ActionReason::Reconnect;
                    }
                    restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                    interruptible_sleep(&running_clone, notify_delay_ms);
                    notify_delay_ms = next_retry_delay(notify_delay_ms);
                    pending_external_change = None;
                    continue;
                };
                external_read_retries = 0;
                let did_unmute = reading.muted;
                let did_fix_volume = volume_needs_fix(reading.normalized_level);
                let old_pct = reading.percent;

                if !external_change_handling && (did_unmute || did_fix_volume) {
                    pending_external_change = Some(now);
                    notify_delay_ms = RETRY_DELAY_MIN_MS;
                    restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                    continue;
                }

                if !did_unmute && !did_fix_volume {
                    if external_change_handling
                        || last_idle_status_update.is_none_or(|updated_at| {
                            now.duration_since(updated_at) >= Duration::from_secs(1)
                        })
                    {
                        let mut s = status_clone.lock().unwrap();
                        s.restore_noop_cycles += 1;
                        s.error = None;
                        s.last_action_reason = if is_periodic_check {
                            ActionReason::PeriodicCheckStable
                        } else {
                            ActionReason::NoRestoreNeeded
                        };
                        last_idle_status_update = Some(now);
                    }
                    notify_delay_ms = RETRY_DELAY_MIN_MS;
                    restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                    pending_external_change = None;
                    last_periodic_check = Some(now);
                    continue;
                }

                let rate_limit_elapsed_ms = last_restore_action
                    .map(|restored_at| now.duration_since(restored_at).as_millis() as u64);
                if external_change_handling
                    && let Some(elapsed_ms) = rate_limit_elapsed_ms
                    && elapsed_ms < restore_rate_limit_ms
                {
                    let wait_ms = restore_rate_limit_ms.saturating_sub(elapsed_ms);
                    restore_rate_limit_ms = next_retry_delay(restore_rate_limit_ms);
                    restore_rate_limit_ms = restore_rate_limit_ms.min(RESTORE_RATE_LIMIT_MAX_MS);
                    {
                        let mut s = status_clone.lock().unwrap();
                        s.last_action_reason = ActionReason::RestoreRateLimited;
                    }
                    match callback_rx.recv_timeout(Duration::from_millis(wait_ms)) {
                        Ok(()) => while callback_rx.try_recv().is_ok() {},
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    continue;
                }

                let current_mic_id = mic.as_ref().expect("mic missing").id.clone();
                let mut effective_volume = match ensure_default_mic_for_write(
                    enumerator,
                    &current_mic_id,
                    false,
                    &callback_tx,
                    callback_context,
                    &status_clone,
                    &mut mic,
                ) {
                    Some(volume) => volume,
                    None => {
                        if external_change_handling {
                            pending_external_change = Some(now);
                            thread::sleep(Duration::from_millis(UNSTABLE_EXTERNAL_RETRY_MS));
                            continue;
                        }
                        restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                        notify_delay_ms = next_retry_delay(notify_delay_ms);
                        continue;
                    }
                };

                {
                    let mut s = status_clone.lock().unwrap();
                    s.restore_attempts += 1;
                    s.last_action_reason = if external_change_handling {
                        ActionReason::ExternalChange
                    } else {
                        ActionReason::PeriodicCheck
                    };
                }
                last_restore_action = Some(Instant::now());

                let mut restore_verified = false;
                let mut write_attempts = 0;
                while write_attempts < 2 {
                    let mut recovery_error = None;

                    if did_unmute && effective_volume.SetMute(false, &callback_context).is_err() {
                        recovery_error = Some("Failed to unmute microphone");
                    }

                    if did_fix_volume
                        && effective_volume
                            .SetMasterVolumeLevelScalar(1.0, &callback_context)
                            .is_err()
                    {
                        recovery_error = Some("Failed to restore microphone volume");
                    }

                    if recovery_error.is_none() {
                        match get_mic_reading(&effective_volume) {
                            Some(readback)
                                if !readback.muted
                                    && !volume_needs_fix(readback.normalized_level) =>
                            {
                                let mut s = status_clone.lock().unwrap();
                                if did_unmute {
                                    s.last_unmute = Some(Instant::now());
                                    s.unmutes_count += 1;
                                }
                                if did_fix_volume {
                                    s.last_volume_fix = Some(Instant::now());
                                    s.fixes_count += 1;
                                }
                                s.error = None;
                                s.last_action_reason = ActionReason::WriteVerified;
                                restore_verified = true;
                                break;
                            }
                            _ => {
                                let mut s = status_clone.lock().unwrap();
                                s.post_write_verify_failures += 1;
                                recovery_error = Some("Post-write verification failed");
                            }
                        }
                    }

                    if recovery_error.is_none() {
                        break;
                    }

                    {
                        let mut s = status_clone.lock().unwrap();
                        s.last_action_reason = ActionReason::WriteFailed;
                        s.write_failures += 1;
                        if write_attempts == 0 {
                            s.write_retries += 1;
                        }
                    }

                    if write_attempts == 0 {
                        write_attempts += 1;
                        match ensure_default_mic_for_write(
                            enumerator,
                            &current_mic_id,
                            true,
                            &callback_tx,
                            callback_context,
                            &status_clone,
                            &mut mic,
                        ) {
                            Some(volume) => {
                                effective_volume = volume;
                                continue;
                            }
                            None => {
                                break;
                            }
                        }
                    }

                    break;
                }

                if restore_verified {
                    set_status_error(&status_clone, None);
                } else {
                    if let Ok(mut s) = status_clone.lock() {
                        s.restore_misses += 1;
                    }
                    set_status_error(
                        &status_clone,
                        Some("Failed to enforce microphone settings".into()),
                    );
                }
                restore_rate_limit_ms = RESTORE_RATE_LIMIT_MS;
                notify_delay_ms = RETRY_DELAY_MIN_MS;

                // Send notification with cooldown; only for restores that
                // actually took effect (failures surface via the tray status).
                if restore_verified
                    && last_notify.is_none_or(|notified_at| {
                        notified_at.elapsed().as_secs() >= NOTIFY_COOLDOWN_SECS
                    })
                {
                    let msg = if did_unmute && did_fix_volume {
                        format!("Unmuted and volume raised from {old_pct}% to 100%")
                    } else if did_unmute {
                        "Microphone was muted - unmuted".into()
                    } else {
                        format!("Volume raised from {old_pct}% to 100%")
                    };
                    let _ = notify_tx.try_send(msg);
                    last_notify = Some(Instant::now());
                }

                pending_external_change = None;
                last_periodic_check = Some(Instant::now());
            }
            if com_inited {
                drop(mic);
                CoUninitialize();
            }
        }
    });

    // Build tray menu
    let menu = Menu::new();
    let device_item = MenuItem::new("Mic: starting...", false, None);
    let volume_item = MenuItem::new("Volume: checking...", false, None);
    let unmute_item = MenuItem::new("Unmute: n/a", false, None);
    let action_item = MenuItem::new("Action: idle", false, None);
    let restore_item = MenuItem::new("Restore: none", false, None);
    let pause_item = CheckMenuItem::new("Paused", true, false, None);
    let startup_item = CheckMenuItem::new("Start with Windows", true, is_in_startup(), None);
    let quit_item = MenuItem::new("Quit", true, None);
    if let Err(err) = menu.append(&device_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&volume_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&unmute_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&action_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&restore_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&PredefinedMenuItem::separator()) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&pause_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&startup_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&PredefinedMenuItem::separator()) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }
    if let Err(err) = menu.append(&quit_item) {
        set_status_error(&status, Some(format!("Failed to build tray menu: {err}")));
        running.store(false, Ordering::Release);
        let _ = monitor_handle.join();
        return;
    }

    let _tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Mic Volume Fix")
        .with_icon(create_icon(true))
        .build()
    {
        Ok(tray) => tray,
        Err(err) => {
            set_status_error(&status, Some(format!("Failed to create tray icon: {err}")));
            running.store(false, Ordering::Release);
            let _ = monitor_handle.join();
            return;
        }
    };

    let menu_receiver = MenuEvent::receiver().clone();
    let mut last_menu_update: Option<Instant> = None;
    let mut last_device_text = String::new();
    let mut last_volume_text = String::new();
    let mut last_unmute_text = String::new();
    let mut last_action_text = String::new();
    let mut last_restore_text = String::new();
    let mut should_quit = false;

    'main: loop {
        // Block until a Win32 message arrives (tray/menu input reacts
        // immediately) or the timeout elapses (menu-stat refresh and toast
        // channel poll), instead of waking 10x/sec to peek.
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            MsgWaitForMultipleObjects(0, std::ptr::null(), 0, MAIN_LOOP_WAIT_MS, QS_ALLINPUT);
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    break 'main;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

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

        if should_quit {
            break 'main;
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

        // Update menu items (at most once per second)
        if last_menu_update.is_none_or(|updated_at| updated_at.elapsed().as_secs() >= 1) {
            last_menu_update = Some(Instant::now());
            if let Ok(s) = status.lock() {
                let device_text = if let Some(ref err) = s.error {
                    format!("Mic: {err}")
                } else if s.device_name.is_empty() {
                    "Mic: no device".to_string()
                } else {
                    format!("Mic: {}", s.device_name)
                };
                if device_text != last_device_text {
                    device_item.set_text(device_text.clone());
                    last_device_text = device_text;
                }

                let volume_text = if let Some(t) = s.last_volume_fix {
                    let ago = format_duration(t.elapsed().as_secs());
                    format!("Volume fixed: {ago} ago (total: {})", s.fixes_count)
                } else {
                    "Volume: no fixes needed yet".to_string()
                };
                if volume_text != last_volume_text {
                    volume_item.set_text(volume_text.clone());
                    last_volume_text = volume_text;
                }

                let unmute_text = if let Some(t) = s.last_unmute {
                    let ago = format_duration(t.elapsed().as_secs());
                    format!("Unmuted: {ago} ago (total: {})", s.unmutes_count)
                } else {
                    "Unmute: no fixes needed yet".to_string()
                };
                if unmute_text != last_unmute_text {
                    unmute_item.set_text(unmute_text.clone());
                    last_unmute_text = unmute_text;
                }

                let action_text = if let Some(ref err) = s.error {
                    format!("Action: {err}")
                } else {
                    format!("Action: {}", s.last_action_reason.label())
                };
                if action_text != last_action_text {
                    action_item.set_text(action_text.clone());
                    last_action_text = action_text;
                }

                let restore_text = format!(
                    "Restore attempts: {} (misses: {}, no-op cycles: {}, reconnects: {}, write retries: {}, write fails: {}, verify fails: {}, callback reg fails: {})",
                    s.restore_attempts,
                    s.restore_misses,
                    s.restore_noop_cycles,
                    s.reconnects,
                    s.write_retries,
                    s.write_failures,
                    s.post_write_verify_failures,
                    s.callback_registration_failures
                );
                if restore_text != last_restore_text {
                    restore_item.set_text(restore_text.clone());
                    last_restore_text = restore_text;
                }
            }
        }
    }

    running.store(false, Ordering::Release);
    if let Err(_join_err) = monitor_handle.join() {
        let _ = _join_err;
    }
}
