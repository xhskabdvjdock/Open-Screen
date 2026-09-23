use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecArea {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    // ---- General / screenshot ----
    pub start_with_windows: bool,
    pub run_in_background: bool,
    pub show_tray_icon: bool,
    pub screenshot_hotkey: String,
    pub ocr_hotkey: String,
    pub default_format: String,
    pub save_location: String,
    pub auto_save: bool,
    pub auto_copy: bool,
    pub capture_cursor: bool,
    pub delay_ms: u64,
    pub history_enabled: bool,
    pub history_limit: usize,
    pub ocr_enabled: bool,
    pub ocr_language: String,
    pub ocr_auto: bool,
    pub ocr_auto_copy_text: bool,
    pub default_pen_color: String,
    pub default_stroke: u32,
    pub default_font_size: u32,
    pub naming_format: String,
    // ---- Recording hotkeys ----
    pub rec_start_hotkey: String,
    pub rec_stop_hotkey: String,
    pub rec_pause_hotkey: String,
    pub rec_resume_hotkey: String,
    pub replay_save_hotkey: String,
    // ---- Recording ----
    pub rec_default_source: String,
    pub rec_monitor: usize,
    pub rec_last_area: Option<RecArea>,
    pub rec_fps: u32,
    pub rec_resolution: String,
    pub rec_custom_w: u32,
    pub rec_custom_h: u32,
    pub rec_preset: String,
    pub rec_quality: String,
    pub rec_bitrate: String,
    pub rec_bitrate_custom: u32,
    pub rec_codec: String,
    pub rec_audio: String,
    pub rec_mic_device: String,
    pub rec_sample_rate: u32,
    pub rec_channels: u32,
    pub rec_cursor: bool,
    pub rec_folder: String,
    pub rec_post_action: String,
    pub rec_history_limit: usize,
    pub rec_power_saving: bool,
    pub rec_countdown: u64,
    pub rec_hw_mode: String,
    // ---- Instant Replay ----
    pub replay_enabled: bool,
    pub replay_autostart: bool,
    pub replay_duration: u64,
    pub replay_preset: String,
    pub replay_audio: String,
    pub replay_post_action: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        let pictures = dirs::picture_dir()
            .map(|p| p.join("Open Screen").to_string_lossy().to_string())
            .unwrap_or_else(|| "Pictures/Open Screen".to_string());
        let videos = dirs::video_dir()
            .map(|p| p.join("Open Screen").to_string_lossy().to_string())
            .unwrap_or_else(|| "Videos/Open Screen".to_string());
        Self {
            start_with_windows: false,
            run_in_background: true,
            show_tray_icon: true,
            screenshot_hotkey: "Ctrl+Shift+S".to_string(),
            ocr_hotkey: "Ctrl+Shift+O".to_string(),
            default_format: "png".to_string(),
            save_location: pictures,
            auto_save: false,
            auto_copy: false,
            capture_cursor: false,
            delay_ms: 0,
            history_enabled: true,
            history_limit: 50,
            ocr_enabled: true,
            ocr_language: "auto".to_string(),
            ocr_auto: false,
            ocr_auto_copy_text: false,
            default_pen_color: "#e81123".to_string(),
            default_stroke: 3,
            default_font_size: 18,
            naming_format: "OpenScreen_{date}_{time}".to_string(),
            // recording
            rec_start_hotkey: "Ctrl+Shift+R".to_string(),
            rec_stop_hotkey: "Ctrl+Shift+R".to_string(),
            rec_pause_hotkey: "Ctrl+Shift+P".to_string(),
            rec_resume_hotkey: "Ctrl+Shift+P".to_string(),
            replay_save_hotkey: "Ctrl+Shift+I".to_string(),
            rec_default_source: "fullscreen".to_string(),
            rec_monitor: 0,
            rec_last_area: None,
            rec_fps: 60,
            rec_resolution: "source".to_string(),
            rec_custom_w: 1920,
            rec_custom_h: 1080,
            rec_preset: "balanced".to_string(),
            rec_quality: "high".to_string(),
            rec_bitrate: "auto".to_string(),
            rec_bitrate_custom: 12,
            rec_codec: "auto".to_string(),
            rec_audio: "system".to_string(),
            rec_mic_device: "".to_string(),
            rec_sample_rate: 48000,
            rec_channels: 2,
            rec_cursor: true,
            rec_folder: videos,
            rec_post_action: "nothing".to_string(),
            rec_history_limit: 25,
            rec_power_saving: false,
            rec_countdown: 3,
            rec_hw_mode: "auto".to_string(),
            // replay
            replay_enabled: false,
            replay_autostart: false,
            replay_duration: 30,
            replay_preset: "standard".to_string(),
            replay_audio: "system".to_string(),
            replay_post_action: "nothing".to_string(),
        }
    }
}

pub fn app_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Open Screen")
}

/// Files shipped INSIDE the installer (Tauri resources): engines and language
/// data install with the app, so no runtime download is needed.
/// Layouts: `<exe_dir>/resources/<rel>` (installed app) or
/// `<crate>/resources/<rel>` (dev run). No AppHandle required.
pub fn bundled_file(rel: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("resources").join(rel);
            if p.exists() {
                return Some(p);
            }
            let p2 = dir.join(rel);
            if p2.exists() {
                return Some(p2);
            }
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join(rel);
    if dev.exists() {
        return Some(dev);
    }
    None
}

pub fn settings_path() -> PathBuf {
    app_dir().join("settings.json")
}

pub fn load_settings() -> AppSettings {
    let fallback = AppSettings::default();
    let p = settings_path();
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(_) => return fallback,
    };
    // Merge saved values over current defaults so old settings files
    // (missing new keys after an update) keep working without reset.
    let mut merged = match serde_json::to_value(&fallback) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return fallback,
    };
    if let Ok(serde_json::Value::Object(saved)) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        for (k, v) in saved {
            merged.insert(k, v);
        }
    }
    match serde_json::from_value::<AppSettings>(serde_json::Value::Object(merged)) {
        Ok(mut s) => {
            if s.history_limit > 100 {
                s.history_limit = 100;
            }
            if s.rec_history_limit > 100 {
                s.rec_history_limit = 100;
            }
            if s.replay_duration < 5 {
                s.replay_duration = 5;
            }
            if s.replay_duration > 600 {
                s.replay_duration = 600;
            }
            s.rec_countdown = match s.rec_countdown {
                0 | 3 | 5 | 10 => s.rec_countdown,
                _ => 3,
            };
            if s.rec_hw_mode != "hw" && s.rec_hw_mode != "sw" {
                s.rec_hw_mode = "auto".to_string();
            }
            s
        }
        Err(_) => fallback,
    }
}

pub fn save_settings(s: &AppSettings) -> Result<(), String> {
    let dir = app_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(settings_path(), bytes).map_err(|e| e.to_string())?;
    Ok(())
}
