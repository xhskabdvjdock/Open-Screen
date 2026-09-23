#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
#[cfg(target_os = "windows")]
mod wgc;
#[cfg(target_os = "windows")]
mod nativerec;
#[cfg(target_os = "windows")]
mod mfhw;
mod mp4info;
mod mp4mux;
mod history;
mod ocr;
mod procutil;
mod recorder;
mod screenshot;
mod settings;

use base64::Engine as _;
use screenshot::{ActiveWindowInfo, CaptureResult, MonitorInfo};
use settings::AppSettings;
use tauri::{
    menu::{Menu, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager, WindowEvent,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// Shared recording / replay runtime (tokio Mutex: nothing runs while idle).
pub struct RecState(pub recorder::Shared);

fn parse_hotkey(s: &str) -> Option<Shortcut> {
    // Accepts forms like "Ctrl+Shift+S", "Control+Shift+O", "Alt+S"
    let lower = s.to_lowercase();
    let parts: Vec<&str> = lower.split('+').map(|p| p.trim()).collect();
    if parts.is_empty() {
        return None;
    }
    let mut mods = Modifiers::empty();
    let mut key_part: Option<&str> = None;
    for p in parts {
        match p {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" => mods |= Modifiers::ALT,
            "super" | "meta" | "win" | "cmd" => mods |= Modifiers::SUPER,
            other => key_part = Some(other),
        }
    }
    let key = key_part?;
    let code = match key {
        "a" => Code::KeyA, "b" => Code::KeyB, "c" => Code::KeyC, "d" => Code::KeyD,
        "e" => Code::KeyE, "f" => Code::KeyF, "g" => Code::KeyG, "h" => Code::KeyH,
        "i" => Code::KeyI, "j" => Code::KeyJ, "k" => Code::KeyK, "l" => Code::KeyL,
        "m" => Code::KeyM, "n" => Code::KeyN, "o" => Code::KeyO, "p" => Code::KeyP,
        "q" => Code::KeyQ, "r" => Code::KeyR, "s" => Code::KeyS, "t" => Code::KeyT,
        "u" => Code::KeyU, "v" => Code::KeyV, "w" => Code::KeyW, "x" => Code::KeyX,
        "y" => Code::KeyY, "z" => Code::KeyZ,
        "0" => Code::Digit0, "1" => Code::Digit1, "2" => Code::Digit2, "3" => Code::Digit3,
        "4" => Code::Digit4, "5" => Code::Digit5, "6" => Code::Digit6, "7" => Code::Digit7,
        "8" => Code::Digit8, "9" => Code::Digit9,
        "f1" => Code::F1, "f2" => Code::F2, "f3" => Code::F3, "f4" => Code::F4,
        "f5" => Code::F5, "f6" => Code::F6, "f7" => Code::F7, "f8" => Code::F8,
        "f9" => Code::F9, "f10" => Code::F10, "f11" => Code::F11, "f12" => Code::F12,
        "printscreen" | "print" => Code::PrintScreen,
        _ => return None,
    };
    Some(Shortcut::new(Some(mods), code))
}

fn show_window(app: &tauri::AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.center();
    }
}

/// Decode the bundled tray PNG into a Tauri image.
/// Fails loudly (setup error) instead of producing an invisible tray.
fn load_tray_icon() -> Result<tauri::image::Image<'static>, String> {
    let bytes = include_bytes!("../icons/32x32.png");
    let rgba = image::load_from_memory(bytes)
        .map_err(|e| format!("tray icon decode failed: {e}"))?
        .to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok(tauri::image::Image::new_owned(rgba.into_raw(), w, h))
}

fn start_capture(app: &tauri::AppHandle, mode: &str) {
    let app_h = app.clone();
    let mode = mode.to_string();
    tauri::async_runtime::spawn(async move {
        for label in ["editor", "settings", "history"] {
            if let Some(w) = app_h.get_webview_window(label) {
                let _ = w.hide();
            }
        }
        // Optional delay (for menus / hover states) before freezing the screen.
        let settings = settings::load_settings();
        if settings.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(settings.delay_ms.min(30_000))).await;
        }
        // Remember the active window BEFORE freezing (the overlay would steal focus).
        let active_window = screenshot::active_window_info().ok();
        // Freeze the whole virtual screen: the overlay shows this frozen
        // full-screen image so the user sees exactly what they select from.
        let frozen = tokio::task::spawn_blocking(screenshot::capture_all).await;
        let Some(overlay) = app_h.get_webview_window("overlay") else {
            return;
        };
        match frozen {
            Ok(Ok(cap)) => {
                let monitors = screenshot::list_monitors().unwrap_or_default();
                let _ = overlay.emit(
                    "openscreen:capture-start",
                    serde_json::json!({
                        "mode": mode,
                        "bg": cap.base64,
                        "bgWidth": cap.width,
                        "bgHeight": cap.height,
                        "monitors": monitors,
                        "activeWindow": active_window,
                    }),
                );
                // NOTE: no fullscreen — the overlay spans the virtual screen
                // (all monitors). Size it HERE (before show) so it appears
                // covering the full screen from the very first frame —
                // never as a small window.
                let _ = overlay.set_fullscreen(false);
                if let Ok(monitors) = screenshot::list_monitors() {
                    if !monitors.is_empty() {
                        let min_x = monitors.iter().map(|m| m.x).min().unwrap_or(0);
                        let min_y = monitors.iter().map(|m| m.y).min().unwrap_or(0);
                        let max_x = monitors
                            .iter()
                            .map(|m| m.x + m.width as i32)
                            .max()
                            .unwrap_or(min_x + 800);
                        let max_y = monitors
                            .iter()
                            .map(|m| m.y + m.height as i32)
                            .max()
                            .unwrap_or(min_y + 600);
                        let _ = overlay.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                            x: min_x,
                            y: min_y,
                        }));
                        let _ = overlay.set_size(tauri::Size::Physical(tauri::PhysicalSize {
                            width: (max_x - min_x).max(800) as u32,
                            height: (max_y - min_y).max(600) as u32,
                        }));
                    }
                }
                let _ = overlay.show();
                let _ = overlay.set_always_on_top(true);
                let _ = overlay.set_focus();
            }
            _ => {
                use tauri_plugin_notification::NotificationExt;
                let _ = app_h
                    .notification()
                    .builder()
                    .title("Open Screen")
                    .body("Screenshot failed: could not capture the screen.")
                    .show();
            }
        }
    });
}

// ---------- Commands ----------

#[tauri::command]
fn monitors() -> Result<Vec<MonitorInfo>, String> {
    screenshot::list_monitors()
}

#[tauri::command]
fn capture_region(x: i32, y: i32, width: u32, height: u32) -> Result<CaptureResult, String> {
    screenshot::capture_region(x, y, width, height)
}

#[tauri::command]
fn capture_monitor(index: usize) -> Result<CaptureResult, String> {
    screenshot::capture_monitor(index)
}

#[tauri::command]
fn capture_all() -> Result<CaptureResult, String> {
    screenshot::capture_all()
}

#[tauri::command]
fn capture_active_window() -> Result<CaptureResult, String> {
    screenshot::capture_active_window()
}

#[tauri::command]
fn active_window_info() -> Result<ActiveWindowInfo, String> {
    screenshot::active_window_info()
}

#[tauri::command]
async fn capture_delayed_ms(mode: String, delay_ms: u64, x: Option<i32>, y: Option<i32>, w: Option<u32>, h: Option<u32>, monitor: Option<usize>) -> Result<CaptureResult, String> {
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms.min(30_000))).await;
    }
    match mode.as_str() {
        "fullscreen" => {
            let idx = monitor.unwrap_or(0);
            tokio::task::spawn_blocking(move || screenshot::capture_monitor(idx))
                .await
                .map_err(|e| e.to_string())?
        }
        "window" => tokio::task::spawn_blocking(screenshot::capture_active_window)
            .await
            .map_err(|e| e.to_string())?,
        "all" => tokio::task::spawn_blocking(screenshot::capture_all)
            .await
            .map_err(|e| e.to_string())?,
        _ => {
            let (x, y, w, h) = (x.unwrap_or(0), y.unwrap_or(0), w.unwrap_or(0), h.unwrap_or(0));
            tokio::task::spawn_blocking(move || screenshot::capture_region(x, y, w, h))
                .await
                .map_err(|e| e.to_string())?
        }
    }
}

#[tauri::command]
fn clipboard_copy_image(base64_png: String) -> Result<(), String> {
    use arboard::ImageData;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_png.trim())
        .map_err(|e| e.to_string())?;
    // Decode any PNG/JPEG into raw RGBA for arboard
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let data = ImageData {
        width: w,
        height: h,
        bytes: rgba.into_raw().into(),
    };
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_image(data).map_err(|_| "Unable to copy screenshot to clipboard.".to_string())?;
    Ok(())
}

#[tauri::command]
fn clipboard_copy_text(text: String) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn save_image_bytes(base64_data: String, path: String) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data.trim())
        .map_err(|e| e.to_string())?;
    let p = std::path::PathBuf::from(&path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "Unable to save screenshot. [Choose another folder]".to_string())?;
    }
    std::fs::write(&p, &bytes).map_err(|_| "Unable to save screenshot. [Choose another folder]".to_string())?;
    Ok(path)
}

#[tauri::command]
fn convert_image(base64_png: String, format: String, quality: Option<u8>) -> Result<serde_json::Value, String> {
    let img = screenshot::decode_base64_png(&base64_png)?;
    let (b64, mime) = screenshot::encode_image_base64(&img, &format, quality.unwrap_or(90))?;
    Ok(serde_json::json!({ "base64": b64, "mime": mime }))
}

#[tauri::command]
fn default_save_dir() -> String {
    screenshot::default_pictures_dir().to_string_lossy().to_string()
}

#[tauri::command]
fn generate_filename(format: String, pattern: Option<String>) -> String {
    let pat = pattern.unwrap_or_else(|| "OpenScreen_{date}_{time}".to_string());
    screenshot::generate_filename(&format, &pat)
}

#[tauri::command]
fn settings_load() -> AppSettings {
    settings::load_settings()
}

#[tauri::command]
fn settings_save(s: AppSettings) -> Result<(), String> {
    // Apply autostart immediately
    settings::save_settings(&s)
}

#[tauri::command]
fn history_list() -> Vec<history::HistoryItem> {
    history::history_list()
}

#[tauri::command]
fn history_add(base64_png: String, width: u32, height: u32, limit: Option<usize>) -> Result<history::HistoryItem, String> {
    history::history_add(&base64_png, width, height, limit.unwrap_or(50))
}

#[tauri::command]
fn history_get(id: String) -> Result<String, String> {
    history::history_get(&id)
}

#[tauri::command]
fn history_delete(id: String) -> Result<(), String> {
    history::history_delete(&id)
}

#[tauri::command]
fn history_clear() -> Result<(), String> {
    history::history_clear()
}

#[tauri::command]
async fn ocr_status() -> ocr::OcrStatus {
    ocr::ocr_status().await
}

#[tauri::command]
async fn ocr_image(base64_png: String, lang: Option<String>) -> Result<ocr::OcrResult, String> {
    ocr::ocr_image_b64(&base64_png, &lang.unwrap_or_else(|| "auto".to_string())).await
}

#[tauri::command]
async fn ocr_install() -> Result<String, String> {
    ocr::install_via_winget().await
}

#[tauri::command]
async fn ocr_ensure_lang(lang: String) -> Result<String, String> {
    ocr::ensure_lang(&lang).await
}

// ---------- Screen recording + Instant Replay ----------

#[tauri::command]
async fn rec_probe() -> Result<recorder::Probe, String> {
    recorder::get_probe(false)
}

#[tauri::command]
async fn rec_probe_refresh() -> Result<recorder::Probe, String> {
    recorder::get_probe(true)
}

/// Native engine needs no download: Media Foundation + WGC + WASAPI ship
/// with Windows. Reports the live backend summary instead.
#[tauri::command]
fn ffmpeg_ensure() -> Result<String, String> {
    Ok(recorder::native_engine_info())
}

#[tauri::command]
fn audio_devices() -> audio::AudioDevices {
    audio::list_audio_devices()
}

/// Native capture diagnostics: live WGC monitor list + a short real capture
/// proving backend, geometry and delivery rate (used by Advanced settings).
#[tauri::command]
async fn native_probe() -> Result<serde_json::Value, String> {
    #[cfg(not(target_os = "windows"))]
    {
        return Err("Windows Graphics Capture requires Windows.".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        let monitors =
            tokio::task::spawn_blocking(|| crate::wgc::list_monitors()).await.map_err(|e| e.to_string())??;
        let stats = tokio::task::spawn_blocking(|| crate::wgc::probe_primary_monitor(2, true, None))
            .await
            .map_err(|e| e.to_string())??;
        Ok(serde_json::json!({ "backend": crate::wgc::backend_name(), "monitors": monitors, "probe": stats }))
    }
}

/// Diagnostics snapshot: system CPU + this app's CPU (the native capture +
/// encode threads live in-process — two samples, honest numbers).
#[tauri::command]
async fn perf_extra() -> Result<serde_json::Value, String> {
    let pid = std::process::id();
    let (sys_cpu, app_cpu) = tokio::task::spawn_blocking(move || {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_all();
        std::thread::sleep(std::time::Duration::from_millis(400));
        sys.refresh_all();
        let sys_cpu = sys.global_cpu_info().cpu_usage();
        let app_cpu = sys
            .process(sysinfo::Pid::from(pid as usize))
            .map(|pr| pr.cpu_usage())
            .unwrap_or(0.0);
        (sys_cpu, app_cpu)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "sysCpu": sys_cpu, "appCpu": app_cpu }))
}

#[tauri::command]
async fn audio_level_test(kind: String, mic: Option<String>) -> Result<Option<f32>, String> {
    let kind = kind.to_lowercase();
    let mic = mic.unwrap_or_default();
    // Blocking WASAPI probe (~1s) off the async runtime.
    tokio::task::spawn_blocking(move || crate::audio::probe_level(&kind, &mic))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn media_info(path: String) -> Result<recorder::MediaInfo, String> {
    Ok(recorder::verify_media(std::path::Path::new(&path)))
}

#[tauri::command]
async fn rec_status(state: tauri::State<'_, RecState>) -> Result<recorder::RecStatus, String> {
    let r = state.0.lock().await;
    Ok(recorder::snapshot(&r))
}

#[tauri::command]
async fn rec_start_area(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<recorder::RecStatus, String> {
    let area = recorder::Area {
        x,
        y,
        w: w.max(64),
        h: h.max(64),
    };
    // Remember for "last area" quick source.
    let mut s = settings::load_settings();
    s.rec_last_area = Some(settings::RecArea {
        x,
        y,
        w: w.max(64),
        h: h.max(64),
    });
    s.rec_default_source = "last_area".to_string();
    let _ = settings::save_settings(&s);
    let shared = state.0.clone();
    let mut r = shared.lock().await;
    r.start_recording(shared.clone(), &app, area).await
}

#[tauri::command]
fn rec_countdown_cancel(state: tauri::State<'_, RecState>) {
    if let Ok(r) = state.0.try_lock() {
        r.countdown_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    } else {
        // Lock held by the starter task: set via blocking lock from a
        // short-lived thread so the cancel never deadlocks.
        let shared = state.0.clone();
        std::thread::spawn(move || {
            shared.blocking_lock().countdown_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
}

#[tauri::command]
async fn rec_start_default(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    if overlay_countdown(&app).await {
        return Err("Cancelled.".to_string());
    }
    let s = settings::load_settings();
    let area = recorder::default_source_area(&s);
    let shared = state.0.clone();
    let mut r = shared.lock().await;
    r.start_recording(shared.clone(), &app, area).await
}

#[tauri::command]
async fn rec_stop(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    let mut r = state.0.lock().await;
    r.stop_recording(&app).await
}

#[tauri::command]
async fn rec_pause(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    let mut r = state.0.lock().await;
    r.pause_recording(&app).await
}

#[tauri::command]
async fn rec_resume(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    let mut r = state.0.lock().await;
    r.resume_recording(&app).await
}

#[tauri::command]
async fn replay_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    let shared = state.0.clone();
    let mut r = shared.lock().await;
    r.replay_start(shared.clone(), &app).await
}

#[tauri::command]
async fn replay_stop(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    let mut r = state.0.lock().await;
    r.replay_stop(&app).await
}

#[tauri::command]
async fn replay_save(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecState>,
) -> Result<recorder::RecStatus, String> {
    let mut r = state.0.lock().await;
    r.replay_save(&app).await
}

#[tauri::command]
fn rec_history_list() -> Vec<recorder::RecHistoryItem> {
    recorder::rec_history_list()
}

#[tauri::command]
fn rec_history_delete(id: String) -> Result<(), String> {
    recorder::rec_history_delete(&id)
}

#[tauri::command]
fn rec_history_clear() -> Result<(), String> {
    recorder::rec_history_clear()
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    open::that(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        procutil::cmd("explorer")
            .arg("/select,")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        open::that(&path).map_err(|e| e.to_string())
    }
}

// Window orchestration from frontend/tray
#[tauri::command]
fn ui_start_capture(app: tauri::AppHandle, mode: Option<String>) {
    start_capture(&app, &mode.unwrap_or_else(|| "region".to_string()));
}

#[tauri::command]
fn ui_open_editor(app: tauri::AppHandle) {
    show_window(&app, "editor");
}

#[tauri::command]
fn ui_open_settings(app: tauri::AppHandle) {
    show_window(&app, "settings");
}

#[tauri::command]
fn ui_open_history(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("history") {
        let _ = w.emit("openscreen:history-refresh", ());
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn op_err(app: &tauri::AppHandle, e: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title("Open Screen")
        .body(e)
        .show();
}

/// On-screen countdown for hotkey/default-source starts.
/// Shows the overlay window with a top-center 3-2-1 banner (driven by the
/// overlay frontend), waits, then hides it and returns `true` when Esc
/// cancelled. Capture starts only AFTER it finishes, so the countdown
/// never appears in the final video.
async fn overlay_countdown(app: &tauri::AppHandle) -> bool {
    use tauri::Emitter;
    let secs = settings::load_settings().rec_countdown;
    let secs = match secs {
        0 | 3 | 5 | 10 => secs,
        _ => 3,
    };
    if secs == 0 {
        return false;
    }
    // Arm the cancel flag, show the overlay, let the frontend count.
    let cancel_flag = {
        let st = app.state::<RecState>();
        let r = st.0.lock().await;
        r.countdown_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        r.countdown_cancel.clone()
    };
    if let Some(w) = app.get_webview_window("overlay") {
        // Size the overlay over the full virtual screen HERE (backend side,
        // like the screenshot flow) so it appears fullscreen from the very
        // first frame — never as a small default-sized window.
        let _ = w.set_fullscreen(false);
        if let Ok(mons) = screenshot::list_monitors() {
            if !mons.is_empty() {
                let min_x = mons.iter().map(|m| m.x).min().unwrap_or(0);
                let min_y = mons.iter().map(|m| m.y).min().unwrap_or(0);
                let max_x = mons.iter().map(|m| m.x + m.width as i32).max().unwrap_or(min_x + 800);
                let max_y = mons.iter().map(|m| m.y + m.height as i32).max().unwrap_or(min_y + 600);
                let _ = w.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                    x: min_x,
                    y: min_y,
                }));
                let _ = w.set_size(tauri::Size::Physical(tauri::PhysicalSize {
                    width: (max_x - min_x).max(800) as u32,
                    height: (max_y - min_y).max(600) as u32,
                }));
            }
        }
        let _ = w.show();
        let _ = w.set_always_on_top(true);
        let _ = w.set_focus();
        let _ = w.emit("openscreen:countdown", serde_json::json!({ "secs": secs }));
    }
    // Sleep in small slices so an Esc-cancel aborts early.
    let steps = secs * 4;
    for _ in 0..steps {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
    }
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.emit("openscreen:countdown-hide", ());
        let _ = w.hide();
    }
    cancel_flag.load(std::sync::atomic::Ordering::SeqCst)
}

/// Start/stop toggle (shared by tray item, Start hotkey and Stop hotkey).
fn rec_toggle(app: &tauri::AppHandle) {
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let st = h.state::<RecState>();
        let shared = st.0.clone();
        let (recording, paused) = {
            let r = shared.lock().await;
            (r.is_recording(), r.is_paused())
        };
        if recording || paused {
            let mut r = shared.lock().await;
            if let Err(e) = r.stop_recording(&h).await {
                drop(r);
                op_err(&h, &e);
            }
        } else {
            // On-screen countdown first (not recorded), then init capture + audio.
            if overlay_countdown(&h).await {
                return; // Esc cancelled
            }
            // User may have started via picker during the wait.
            {
                let r = shared.lock().await;
                if r.is_recording() || r.is_paused() {
                    return;
                }
            }
            let s = settings::load_settings();
            let area = recorder::default_source_area(&s);
            let mut r = shared.lock().await;
            if let Err(e) = r.start_recording(shared.clone(), &h, area).await {
                drop(r);
                op_err(&h, &e);
            }
        }
    });
}

fn rec_pause_toggle(app: &tauri::AppHandle) {
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let st = h.state::<RecState>();
        let shared = st.0.clone();
        let (recording, paused) = {
            let r = shared.lock().await;
            (r.is_recording(), r.is_paused())
        };
        let mut r = shared.lock().await;
        let res = if paused {
            r.resume_recording(&h).await
        } else if recording {
            r.pause_recording(&h).await
        } else {
            return;
        };
        if let Err(e) = res {
            drop(r);
            op_err(&h, &e);
        }
    });
}

fn replay_toggle(app: &tauri::AppHandle) {
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let st = h.state::<RecState>();
        let shared = st.0.clone();
        let on = {
            let r = shared.lock().await;
            r.replay_state() != "off"
        };
        let mut r = shared.lock().await;
        let res = if on {
            r.replay_stop(&h).await
        } else {
            r.replay_start(shared.clone(), &h).await
        };
        if let Err(e) = res {
            drop(r);
            op_err(&h, &e);
        }
    });
}

/// Tray "Start Recording": opens the fullscreen picker in Record mode so the
/// user chooses the area visually. Hotkey = instant start with default source.
fn tray_rec_toggle(app: &tauri::AppHandle) {
    let active = app
        .try_state::<RecState>()
        .map(|st| {
            let r = st.0.blocking_lock();
            r.is_recording() || r.is_paused()
        })
        .unwrap_or(false);
    if active {
        rec_toggle(app);
    } else {
        start_capture(app, "record");
    }
}

fn replay_save_now(app: &tauri::AppHandle) {    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let st = h.state::<RecState>();
        let shared = st.0.clone();
        let mut r = shared.lock().await;
        if let Err(e) = r.replay_save(&h).await {
            drop(r);
            op_err(&h, &e);
        }
    });
}

async fn graceful_quit(app: tauri::AppHandle) {
    if let Some(st) = app.try_state::<RecState>() {
        let shared = st.0.clone();
        let mut r = shared.lock().await;
        if r.is_recording() || r.is_paused() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(12),
                r.stop_recording(&app),
            )
            .await;
        }
        if r.replay_state() != "off" {
            let _ = r.replay_stop(&app).await;
        }
        drop(r);
    }
    app.exit(0);
}

#[tauri::command]
async fn ui_quit(app: tauri::AppHandle) {
    graceful_quit(app).await;
}

/// Rebuild the tray menu from live recording/replay state.
/// Called after every state change so labels + enabled states are always true.
pub(crate) fn rebuild_tray(app: &tauri::AppHandle, st: &recorder::RecStatus) {
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let rec_active = st.recording || st.paused;
    let rec_label = if rec_active { "Stop Recording" } else { "Start Recording" };
    let pause_label = if st.paused {
        "Resume Recording"
    } else {
        "Pause Recording"
    };
    let replay_label = if st.replay == "off" {
        "Start Instant Replay"
    } else {
        "Stop Instant Replay"
    };
    let m = |id: &str, text: &str, enabled: bool| {
        MenuItemBuilder::new(text)
            .id(id)
            .enabled(enabled)
            .build(app)
    };
    let menu = (|| -> Result<Menu<tauri::Wry>, tauri::Error> {
        Menu::with_items(
            app,
            &[
                &m("title", "Open Screen", false)?,
                &m("take", "Screenshot", true)?,
                &m("ocr", "OCR", true)?,
                &m("rec_toggle", rec_label, true)?,
                &m("rec_pause", pause_label, rec_active)?,
                &m("replay_toggle", replay_label, !rec_active)?,
                &m("replay_save", "Save Instant Replay", st.replay == "ready")?,
                &m("history", "Recording History", true)?,
                &m("settings", "Settings", true)?,
                &m("quit", "Quit", true)?,
            ],
        )
    })();
    if let Ok(menu) = menu {
        let _ = tray.set_menu(Some(menu));
    }
}

/// Returns human-readable descriptions of hotkeys that could NOT be
/// registered (bad format or already taken by the OS/another app).
fn register_hotkeys(app: &tauri::AppHandle, s: &AppSettings) -> Vec<String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let mut failed: Vec<String> = vec![];
    // Helper: parse + register, recording any failure with its label.
    macro_rules! reg {
        ($label:expr, $key:expr, $handler:expr) => {
            match parse_hotkey($key) {
                Some(sc) => {
                    if gs.on_shortcut(sc, $handler).is_err() {
                        failed.push(format!("{} ({}) is already in use by Windows or another app.", $label, $key));
                    }
                }
                None => {
                    failed.push(format!("{} ({}) has an unsupported format — use like Ctrl+Shift+S.", $label, $key));
                }
            }
        };
    }
    let app_h = app.clone();
    reg!("Screenshot", &s.screenshot_hotkey, move |_app, _sc, event| {
        if event.state == ShortcutState::Pressed {
            start_capture(&app_h, "region");
        }
    });
    if s.ocr_enabled {
        let app_h2 = app.clone();
        reg!("OCR", &s.ocr_hotkey, move |_app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                start_capture(&app_h2, "ocr");
            }
        });
    }
    // Recording hotkeys. Start/Stop (and Pause/Resume) may intentionally share
    // one key as a toggle — register shared keys only once.
    let eq = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    if eq(&s.rec_start_hotkey, &s.rec_stop_hotkey) {
        reg!("Start/Stop recording", &s.rec_start_hotkey, move |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                rec_toggle(app);
            }
        });
    } else {
        reg!("Start recording", &s.rec_start_hotkey, move |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                // Start only (never stops an active recording).
                let h = app.clone();
                tauri::async_runtime::spawn(async move {
                    let st = h.state::<RecState>();
                    let shared = st.0.clone();
                    let active = {
                        let r = shared.lock().await;
                        r.is_recording() || r.is_paused()
                    };
                    if active {
                        return;
                    }
                    if overlay_countdown(&h).await {
                        return; // Esc cancelled
                    }
                    {
                        let r = shared.lock().await;
                        if r.is_recording() || r.is_paused() {
                            return;
                        }
                    }
                    let s = settings::load_settings();
                    let area = recorder::default_source_area(&s);
                    let mut r = shared.lock().await;
                    if let Err(e) = r.start_recording(shared.clone(), &h, area).await {
                        drop(r);
                        op_err(&h, &e);
                    }
                });
            }
        });
        reg!("Stop recording", &s.rec_stop_hotkey, move |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                let h = app.clone();
                tauri::async_runtime::spawn(async move {
                    let st = h.state::<RecState>();
                    let shared = st.0.clone();
                    let mut r = shared.lock().await;
                    if r.is_recording() || r.is_paused() {
                        if let Err(e) = r.stop_recording(&h).await {
                            drop(r);
                            op_err(&h, &e);
                        }
                    }
                });
            }
        });
    }
    if eq(&s.rec_pause_hotkey, &s.rec_resume_hotkey) {
        reg!("Pause/Resume recording", &s.rec_pause_hotkey, move |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                rec_pause_toggle(app);
            }
        });
    } else {
        reg!("Pause recording", &s.rec_pause_hotkey, move |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                rec_pause_toggle(app);
            }
        });
        reg!("Resume recording", &s.rec_resume_hotkey, move |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                rec_pause_toggle(app);
            }
        });
    }
    reg!("Save Instant Replay", &s.replay_save_hotkey, move |app, _sc, event| {
        if event.state == ShortcutState::Pressed {
            replay_save_now(app);
        }
    });
    failed
}

#[tauri::command]
fn hotkeys_apply(app: tauri::AppHandle, s: AppSettings) -> Result<Vec<String>, String> {
    Ok(register_hotkeys(&app, &s))
}

fn main() {
    let settings = settings::load_settings();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app, "editor");
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().with_handler(|app, _shortcut, event| {
            // Fallback no-op; real handlers registered per-shortcut in register_hotkeys.
            if event.state == ShortcutState::Pressed {
                let _ = app;
            }
        }).build())
        .invoke_handler(tauri::generate_handler![
            monitors,
            capture_region,
            capture_monitor,
            capture_all,
            capture_active_window,
            active_window_info,
            capture_delayed_ms,
            clipboard_copy_image,
            clipboard_copy_text,
            save_image_bytes,
            convert_image,
            default_save_dir,
            generate_filename,
            settings_load,
            settings_save,
            history_list,
            history_add,
            history_delete,
            history_clear,
            history_get,
            ocr_status,
            ocr_image,
            ocr_install,
            ocr_ensure_lang,
            rec_probe,
            rec_probe_refresh,
            ffmpeg_ensure,
            audio_devices,
            audio_level_test,
            media_info,
            native_probe,
            perf_extra,
            rec_status,
            rec_countdown_cancel,
            rec_start_area,
            rec_start_default,
            rec_stop,
            rec_pause,
            rec_resume,
            replay_start,
            replay_stop,
            replay_save,
            rec_history_list,
            rec_history_delete,
            rec_history_clear,
            open_path,
            reveal_in_folder,
            ui_start_capture,
            ui_open_editor,
            ui_open_settings,
            ui_open_history,
            ui_quit,
            hotkeys_apply,
        ])
        .on_window_event(|window, event| {
            // Keep app alive in tray: hide instead of closing auxiliary windows.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let label = window.label();
                if label == "overlay" || label == "editor" || label == "settings" || label == "history" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        });

    let app_settings = settings.clone();
    let runner = builder
        .manage(RecState(recorder::shared()))
        .setup(move |app| {
            // Idle tray (icon explicit — invisible tray risk). Menu is filled
            // in by rebuild_tray from live state.
            let tray_image = load_tray_icon()?;
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tray_image)
                .tooltip("Open Screen")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "take" => start_capture(app, "region"),
                    "ocr" => start_capture(app, "ocr"),
                    "rec_toggle" => tray_rec_toggle(app),
                    "rec_pause" => rec_pause_toggle(app),
                    "replay_toggle" => replay_toggle(app),
                    "replay_save" => replay_save_now(app),
                    "history" => show_window(app, "history"),
                    "settings" => show_window(app, "settings"),
                    "quit" => {
                        let h = app.clone();
                        tauri::async_runtime::spawn(async move {
                            graceful_quit(h).await;
                        });
                    }
                    _ => {}
                })
                .build(app)?;

            register_hotkeys(app.handle(), &app_settings);

            // Lightweight startup: tray + hotkeys only. No OCR preload,
            // no history preload, no capture pipeline. Clean stale temp
            // recording data left by a previous session.
            recorder::cleanup_temp();
            {
                let st = app.state::<RecState>();
                let idle = {
                    let r = st.0.blocking_lock();
                    recorder::snapshot(&r)
                };
                rebuild_tray(app.handle(), &idle);
            }
            if app_settings.replay_autostart {
                let h = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let st = h.state::<RecState>();
                    let shared = st.0.clone();
                    let mut r = shared.lock().await;
                    let _ = r.replay_start(shared.clone(), &h).await;
                });
            }
            Ok(())
        });

    // `open` helper for open_path command
    runner
        .run(tauri::generate_context!())
        .expect("failed to run Open Screen");
}

// tiny `open` shim to avoid an extra dependency
mod open {
    pub fn that(path: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        crate::procutil::cmd("cmd")
            .args(["/C", "start", "", path])
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(format!("Cannot open {path} on this platform"))
        }
    }
}
