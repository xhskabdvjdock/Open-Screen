//! Screen recording + Instant Replay backend.
//!
//! Strategy (lightweight, local-first):
//! - Video capture/encode via ffmpeg (probed, one-time lazy download like Tesseract).
//! - Hardware encoding preferred (NVENC > QuickSync > AMF > x264 software fallback).
//! - Normal recording: direct MP4 partials; pause/resume = stop/start partials, concat on stop.
//! - Instant Replay: ffmpeg segment muxer (rolling files) + supervisor deleting old
//!   segments; save = concat in-window segments. Temp only, never in user folders.
//! - Nothing runs (no capture, no encoder, no audio) while recording/replay are off.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::settings::{self, AppSettings};

/// Debug-build pipeline log (never screen/audio content â€” config only).
macro_rules! dbg_pipe {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        eprintln!("[open-screen] {}", format!($($arg)*));
    };
}

// ---------------------------------------------------------------------------
// Native engine: Windows Graphics Capture + Media Foundation + WASAPI.
// No external processes, no downloads, no bundled binaries â€” the engine IS
// the OS. Everything below reports what is ACTUALLY present on this machine.
// ---------------------------------------------------------------------------

/// The one and only capture backend. No fallback chain to misreport.
pub const NATIVE_CAPTURE_API: &str = "Windows Graphics Capture";

// ---------------------------------------------------------------------------
// Probe (capture backend + encoder MFTs + audio devices), cached
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    /// Legacy key kept for UI compat; always empty (no external binary).
    pub ffmpeg: String,
    pub version: String,
    /// Real encoder MFT friendly names present on THIS machine
    /// (e.g. "NVIDIA H.264 Encoder MFT", "H264 Encoder MFT").
    pub h264: Vec<String>,
    pub hevc: Vec<String>,
    /// AV1 hardware encode is not offered (no inbox AV1 encoder MFT).
    pub av1: Vec<String>,
    pub audio_devices: Vec<String>,
    pub system_hint: Option<String>,
    pub capture_api: String,
    pub has_ddagrab: bool,
    pub wasapi_mics: Vec<String>,
    pub wasapi_system: Option<String>,
}

fn probe_path() -> PathBuf {
    settings::app_dir().join("native_probe.json")
}

fn run_probe() -> Probe {
    let mut p = Probe {
        version: "native".to_string(),
        capture_api: NATIVE_CAPTURE_API.to_string(),
        ..Default::default()
    };
    #[cfg(target_os = "windows")]
    {
        // Media Foundation encoder MFTs that REALLY exist here. Hardware
        // first, inbox software always as fallback â€” never offer ghosts.
        let hw = crate::mfhw::probe_hw_encoders();
        p.h264.extend(hw.h264_hw.iter().cloned());
        p.hevc.extend(hw.hevc_hw.iter().cloned());
        for n in crate::mfhw::probe_sw("h264") {
            if !p.h264.iter().any(|e| e == &n) {
                p.h264.push(n);
            }
        }
        for n in crate::mfhw::probe_sw("hevc") {
            if !p.hevc.iter().any(|e| e == &n) {
                p.hevc.push(n);
            }
        }
        let wad = crate::audio::list_audio_devices();
        p.wasapi_mics = wad.mics;
        p.wasapi_system = wad.system.clone();
        // WASAPI loopback needs no "Stereo Mix" â€” the render endpoint IS the
        // system-audio source.
        p.system_hint = wad.system;
    }
    p
}

pub fn get_probe(refresh: bool) -> Result<Probe, String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = refresh;
        return Err("Screen recording requires Windows.".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        if !refresh {
            if let Ok(bytes) = std::fs::read(probe_path()) {
                if let Ok(mut p) = serde_json::from_slice::<Probe>(&bytes) {
                    if p.version == "native" && p.capture_api == NATIVE_CAPTURE_API {
                        // MFT/WASAPI sets only grow: refresh cheaply merges live
                        // audio devices (handles hot-plugged microphones).
                        let live = crate::audio::list_audio_devices();
                        if !live.mics.is_empty() {
                            p.wasapi_mics = live.mics;
                        }
                        if live.system.is_some() {
                            p.wasapi_system = live.system;
                            p.system_hint = p.wasapi_system.clone();
                        }
                        return Ok(std::mem::take(&mut p));
                    }
                }
            }
        }
        let p = run_probe();
        let _ = std::fs::create_dir_all(settings::app_dir());
        if let Ok(bytes) = serde_json::to_vec_pretty(&p) {
            let _ = std::fs::write(probe_path(), bytes);
        }
        Ok(p)
    }
}

/// A friendly name is a HARDWARE MFT when it is not the inbox software one.
/// (Inbox: "H264 Encoder MFT". Anything else, e.g. "NVIDIA H.264 Encoder
/// MFT", is vendor hardware.)
fn is_hw_mft_name(n: &str) -> bool {
    let l = n.to_lowercase();
    !(l == "h264 encoder mft" || l.contains("microsoft"))
}

pub fn encoder_label(probe: &Probe, codec: &str) -> String {
    let pool = match codec {
        "hevc" => &probe.hevc,
        _ => &probe.h264,
    };
    if let Some(n) = pool.iter().find(|n| is_hw_mft_name(n)) {
        // Vendor from the live MFT enumeration (not a hardcoded table).
        let vendor = crate::mfhw::probe_hw_encoders().vendor().unwrap_or_else(|| n.clone());
        return format!("Hardware â€” {vendor}");
    }
    "Software (CPU)".to_string()
}

#[derive(Debug, Clone)]
struct EncCand {
    codec: crate::nativerec::NativeCodec,
    hw: bool,
    label: String,
}

/// Native codec choice from the MFTs actually present: hardware MFTs win,
/// inbox software is the fallback. Returns (codec, is_hw, label).
/// `hw_mode`: "hw" = refuse software (error when absent), "sw" = force the
/// inbox software MFT, "auto" = hardware first, software fallback.
/// AV1 is never offered (no inbox AV1 encoder MFT exists).
fn pick_native_codec(
    probe: &Probe,
    codec: &str,
    hw_mode: &str,
) -> Result<(EncCand, Option<String>), String> {
    use crate::nativerec::NativeCodec;
    // Requested codec, or H.264 when the request names something absent
    // (HEVC with no HEVC MFT, or AV1 which has no inbox encoder at all).
    let mut want = match codec {
        "hevc" if !probe.hevc.is_empty() => "hevc",
        "hevc" | "av1" => "h264",
        _ => "h264",
    };
    let mut note = if want != codec {
        Some(format!("{codec} unavailable â€” using H.264 instead."))
    } else {
        None
    };
    let pool = if want == "hevc" { &probe.hevc } else { &probe.h264 };
    let pick_hw = pool.iter().any(|n| is_hw_mft_name(n));
    let pick_sw = pool.iter().any(|n| !is_hw_mft_name(n));
    let (native, hw) = match hw_mode {
        "hw" => {
            if pick_hw {
                (if want == "hevc" { NativeCodec::Hevc } else { NativeCodec::H264 }, true)
            } else {
                return Err("Hardware encoding requested but no hardware encoder MFT found. Switch to Auto or Software.".to_string());
            }
        }
        "sw" => {
            if pick_sw {
                (if want == "hevc" { NativeCodec::Hevc } else { NativeCodec::H264 }, false)
            } else if want == "hevc" && !probe.h264.is_empty() {
                want = "h264";
                note = Some("HEVC software MFT missing â€” using H.264.".to_string());
                (NativeCodec::H264, false)
            } else {
                return Err("Software encoder MFT missing â€” cannot record.".to_string());
            }
        }
        _ => {
            if pick_hw {
                (if want == "hevc" { NativeCodec::Hevc } else { NativeCodec::H264 }, true)
            } else if pick_sw {
                (if want == "hevc" { NativeCodec::Hevc } else { NativeCodec::H264 }, false)
            } else if want == "hevc" && !probe.h264.is_empty() {
                // H.264 (inbox MFT) is the final safety net.
                want = "h264";
                note = Some("HEVC unavailable â€” using H.264.".to_string());
                let hw2 = probe.h264.iter().any(|n| is_hw_mft_name(n));
                (NativeCodec::H264, hw2)
            } else {
                return Err("No video encoder MFT found on this system.".to_string());
            }
        }
    };
    Ok((
        EncCand { codec: native, hw, label: encoder_label(probe, want) },
        note,
    ))
}

// ---------------------------------------------------------------------------
// Native engine status: nothing to download â€” Media Foundation, WGC and
// WASAPI ship with Windows. Kept as a command so the UI can report it.
// ---------------------------------------------------------------------------

/// Human-readable native engine summary for Settings.
pub fn native_engine_info() -> String {
    #[cfg(target_os = "windows")]
    {
        match get_probe(false) {
            Ok(p) => {
                let enc = if !p.h264.is_empty() || !p.hevc.is_empty() {
                    encoder_label(&p, "auto")
                } else {
                    "no encoder MFT".to_string()
                };
                format!("{} Â· {} Â· built-in (no download needed)", NATIVE_CAPTURE_API, enc)
            }
            Err(e) => e,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        "Screen recording requires Windows.".to_string()
    }
}

// ---------------------------------------------------------------------------
// Params resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub area: Area,
    /// Resolved codec actually used: "h264" or "hevc" (never av1).
    pub codec: String,
    pub codec_note: Option<String>,
    pub out_w: u32,
    pub out_h: u32,
    pub fps: u32,
    pub bitrate_k: u32,
    pub bitrate_bps: u32,
    pub hw: bool,
    pub encoder_label: String,
    pub audio: String,
    pub mic: Option<String>,
    pub audio_note: Option<String>,
    pub sample_rate: u32,
    pub channels: u32,
    pub cursor: bool,
    pub power_note: Option<String>,
}

fn even(n: u32) -> u32 {
    (n.max(2) / 2) * 2
}

fn bitrate_for(quality: &str, height: u32) -> u32 {
    // kbps ladder tuned so 1080p60 never looks like upscaled 720p.
    // Balanced 1080p60 ~= 12M, Quality ~= 16M, VeryHigh ~= 20-24M.
    match quality {
        "low" => {
            if height >= 1080 {
                6000
            } else if height >= 720 {
                4000
            } else {
                2000
            }
        }
        "high" => {
            if height >= 1080 {
                16000
            } else if height >= 720 {
                12000
            } else {
                6000
            }
        }
        "veryhigh" => {
            if height >= 1080 {
                24000
            } else if height >= 720 {
                18000
            } else {
                9000
            }
        }
        _ => {
            // balanced
            if height >= 1080 {
                12000
            } else if height >= 720 {
                8000
            } else {
                4000
            }
        }
    }
}

fn resolve_audio(
    s: &AppSettings,
    probe: &Probe,
    replay: bool,
) -> (String, Option<String>, Option<String>) {
    let mode = if replay { s.replay_audio.clone() } else { s.rec_audio.clone() };
    let mode = mode.to_lowercase();
    // Prefer real WASAPI mics; fall back to dshow names for compat.
    let mic_pool: Vec<String> = if !probe.wasapi_mics.is_empty() {
        probe.wasapi_mics.clone()
    } else {
        probe
            .audio_devices
            .iter()
            .filter(|d| {
                let l = d.to_lowercase();
                !(l.contains("stereo mix")
                    || l.contains("what u hear")
                    || l.contains("loopback")
                    || l.contains("wave out"))
            })
            .cloned()
            .collect()
    };
    let pick_mic = || -> Option<String> {
        if !s.rec_mic_device.trim().is_empty()
            && (mic_pool.iter().any(|d| d == &s.rec_mic_device)
                || probe.audio_devices.iter().any(|d| d == &s.rec_mic_device))
        {
            return Some(s.rec_mic_device.clone());
        }
        mic_pool
            .first()
            .cloned()
            .or_else(|| probe.audio_devices.first().cloned())
    };
    // System audio = WASAPI loopback (always available when a render endpoint
    // exists â€” no Stereo Mix needed). Missing endpoints degrade to a note,
    // never to silent failure.
    let sys_ok = probe.wasapi_system.is_some();
    match mode.as_str() {
        "mic" | "microphone" => ("mic".to_string(), pick_mic(), None),
        "system" if sys_ok => ("system".to_string(), None, None),
        "both" if sys_ok => ("both".to_string(), pick_mic(), None),
        "system" => (
            "none".to_string(),
            None,
            Some("System audio endpoint not found â€” recording video only.".to_string()),
        ),
        "both" => (
            "none".to_string(),
            None,
            Some("Audio endpoints not found â€” recording video only.".to_string()),
        ),
        _ => ("none".to_string(), None, None),
    }
}

fn resolve_params(
    s: &AppSettings,
    probe: &Probe,
    area: Area,
    replay: bool,
) -> Result<Params, String> {
    // FPS (never 120 â€” only reliably supported options).
    let mut fps = if replay { 30 } else { s.rec_fps };
    if replay {
        fps = match s.replay_preset.as_str() {
            "quick" => 30,
            "long" => 60,
            _ => 60, // standard
        };
    }
    fps = match fps {
        24 | 30 | 60 => fps,
        _ => 60,
    };
    // Resolution â€” QUALITY FIRST: Capture Resolution = Output Resolution.
    // Never record low then upscale: out is always <= area (clamped).
    let (res_key, cw, ch) = if replay {
        match s.replay_preset.as_str() {
            "quick" => ("720p".to_string(), 0, 0),
            _ => ("1080p".to_string(), 0, 0),
        }
    } else {
        (s.rec_resolution.clone(), s.rec_custom_w, s.rec_custom_h)
    };
    let mut out_h = match res_key.as_str() {
        "1080p" => 1080,
        "720p" => 720,
        "480p" => 480,
        "custom" => ch.max(240).min(2160),
        _ => area.h, // source
    };
    // Never upscale â€” for presets AND custom. If the area is smaller than
    // the preset, keep the area (capture == output, no fake upscale).
    if out_h > area.h {
        out_h = area.h;
    }
    let mut out_w = if res_key == "custom" && !replay {
        let want_w = cw.max(320).min(3840);
        // Preserve aspect from area height, then clamp to area (no upscale).
        let aspect_w = ((area.w as u64 * out_h as u64) / area.h.max(1) as u64) as u32;
        want_w.min(aspect_w).min(area.w)
    } else if res_key == "source" {
        area.w
    } else {
        ((area.w as u64 * out_h as u64) / area.h.max(1) as u64) as u32
    };
    // For presets the width follows height proportionally; also never upscale.
    if res_key != "source" && res_key != "custom" && out_w > area.w {
        out_w = area.w;
        out_h = area.h;
    }
    out_w = even(out_w);
    out_h = even(out_h);
    // Quality / bitrate.
    let (quality, bitrate_k) = if replay {
        match s.replay_preset.as_str() {
            "quick" => ("low".to_string(), bitrate_for("low", out_h)),
            "long" => ("high".to_string(), bitrate_for("high", out_h)),
            _ => ("high".to_string(), bitrate_for("high", out_h)),
        }
    } else {
        let q = match s.rec_preset.as_str() {
            "battery" => "low".to_string(),
            "quality" => "high".to_string(),
            "custom" => s.rec_quality.clone(),
            _ => "high".to_string(), // balanced
        };
        let b = if s.rec_preset == "custom" && s.rec_bitrate != "auto" {
            if s.rec_bitrate == "custom" {
                s.rec_bitrate_custom.clamp(1, 100) * 1000
            } else {
                s.rec_bitrate.parse::<u32>().unwrap_or(8).clamp(1, 100) * 1000
            }
        } else {
            bitrate_for(&q, out_h)
        };
        (q, b)
    };
    let _ = quality;
    // Native codec from the MFTs actually present (HEVC only when an HEVC
    // MFT exists; AV1 is never offered â€” no inbox encoder). H.264 default.
    let want_codec = if replay {
        "h264"
    } else {
        match s.rec_codec.as_str() {
            "hevc" => "hevc",
            _ => "h264",
        }
    };
    let hw_mode = hw_mode_setting();
    let (cand, codec_note) = pick_native_codec(probe, want_codec, &hw_mode)?;
    // Audio (WASAPI loopback + mic; labels only).
    let (audio, mic, audio_note) = resolve_audio(s, probe, replay);
    // Power saving overrides (real: FPS cap + lower bitrate).
    let mut power_note = None;
    let mut fps_out = fps;
    let mut bitrate_out = bitrate_k;
    if !replay && s.rec_power_saving {
        fps_out = fps.min(30);
        bitrate_out = bitrate_k.min(bitrate_for("low", out_h));
        power_note = Some("Power saving active: 30 FPS, lower bitrate.".to_string());
    }
    Ok(Params {
        area: Area {
            x: area.x,
            y: area.y,
            w: even(area.w),
            h: even(area.h),
        },
        codec: match cand.codec {
            crate::nativerec::NativeCodec::Hevc => "hevc".to_string(),
            _ => "h264".to_string(),
        },
        codec_note,
        out_w,
        out_h,
        fps: fps_out,
        bitrate_k: bitrate_out,
        bitrate_bps: bitrate_out * 1000,
        hw: cand.hw,
        encoder_label: cand.label,
        audio,
        mic,
        audio_note,
        sample_rate: if s.rec_sample_rate == 44100 { 44100 } else { 48000 },
        channels: if s.rec_channels == 1 { 1 } else { 2 },
        cursor: s.rec_cursor,
        power_note,
    })
}

// ---------------------------------------------------------------------------
// Recorder state (all live numbers come from atomics + file metadata â€”
// no subprocess scraping)
// ---------------------------------------------------------------------------

/// One live native capture session (exactly one monitor) plus its file.
struct LiveSeg {
    session: crate::nativerec::NativeSession,
    path: PathBuf,
    monitor_idx: usize,
}

/// Finished segment files, grouped per rotation (one entry per monitor).
#[derive(Clone)]
struct SegSet {
    files: Vec<(usize, PathBuf)>,
    finished_at: std::time::SystemTime,
}

struct ActiveRec {
    /// Live sessions (1 normally; N for all-screens).
    segs: Vec<LiveSeg>,
    /// Finished parts awaiting the stop-time merge.
    parts: Vec<SegSet>,
    pump: Option<crate::audio::PcmPump>,
    workdir: PathBuf,
    active_ms: u64,
    active_start: Option<Instant>,
    params: Params,
    final_name: String,
    base_frames: u64,
    /// Captured (pushed) frames at finalize, for duplicate accounting.
    final_frames: u64,
    /// Watchdog already reported an unexpected capture death (report once).
    watch_noted: bool,
}

struct ActiveReplay {
    /// Currently-open segment set (multi-monitor fan-out).
    segs: Vec<LiveSeg>,
    pump: Option<crate::audio::PcmPump>,
    /// Closed segment sets (rolling window pruned by mtime).
    closed: Vec<SegSet>,
    segdir: PathBuf,
    started_at: Instant,
    duration_s: u64,
    params: Params,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    /// Watchdog already reported an unexpected buffer death (report once).
    watch_noted: bool,
}

pub struct Recorder {
    rec: Option<ActiveRec>,
    replay: Option<ActiveReplay>,
    /// True while a replay save is finalizing (state "saving").
    replay_saving: bool,
    rec_icon: Option<(Vec<u8>, u32, u32)>,
    timer_gen: Arc<AtomicU64>,
    timer_handle: Option<tokio::task::AbortHandle>,
    last_message: String,
    /// Set by the overlay (Esc) to abort a pending on-screen countdown.
    pub countdown_cancel: Arc<AtomicBool>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            rec: None,
            replay: None,
            replay_saving: false,
            rec_icon: None,
            timer_gen: Arc::new(AtomicU64::new(0)),
            timer_handle: None,
            last_message: String::new(),
            countdown_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.rec.as_ref().map(|r| !r.segs.is_empty()).unwrap_or(false)
    }

    pub fn is_paused(&self) -> bool {
        match &self.rec {
            Some(r) => r.segs.is_empty(),
            None => false,
        }
    }

    pub fn replay_state(&self) -> &'static str {
        if self.replay_saving {
            return "saving";
        }
        match &self.replay {
            None => "off",
            Some(r) => {
                if r.started_at.elapsed().as_secs() < 3 {
                    "starting"
                } else {
                    "ready"
                }
            }
        }
    }
}

pub type Shared = Arc<tokio::sync::Mutex<Recorder>>;

pub fn shared() -> Shared {
    Arc::new(tokio::sync::Mutex::new(Recorder::new()))
}

// ---------------------------------------------------------------------------
// Status (for tray + frontend)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecStatus {
    pub recording: bool,
    pub paused: bool,
    pub replay: String,
    pub replay_ready_s: u64,
    pub replay_duration: u64,
    pub elapsed_s: u64,
    pub frames: u64,
    pub fps: u32,
    pub actual_fps: f32,
    pub width: u32,
    pub height: u32,
    pub encoder: String,
    pub encoder_label: String,
    pub capture_api: String,
    pub size_bytes: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub sys_db: Option<f32>,
    pub mic_db: Option<f32>,
    pub audio_status: String,
    pub perf_warning: Option<String>,
    pub message: String,
    pub ffmpeg: bool,
    pub ffmpeg_path: Option<String>,
    pub power_note: Option<String>,
}

pub(crate) fn snapshot(rec: &Recorder) -> RecStatus {
    status_of(rec)
}

fn audio_status_text(audio: &str, sys_db: Option<f32>, mic_db: Option<f32>) -> String {
    match audio {
        "both" => format!(
            "sys {} Â· mic {}",
            if sys_db.is_some() { "on" } else { "silent" },
            if mic_db.is_some() { "on" } else { "silent" }
        ),
        "system" => {
            if sys_db.is_some() {
                "system on".to_string()
            } else {
                "system silent".to_string()
            }
        }
        "mic" | "microphone" => {
            if mic_db.is_some() {
                "mic on".to_string()
            } else {
                "mic silent".to_string()
            }
        }
        _ => "no audio".to_string(),
    }
}

fn perf_warning(target_fps: u32, actual: f32, elapsed_s: u64) -> Option<String> {
    if elapsed_s < 5 || target_fps == 0 {
        return None;
    }
    if actual > 0.0 && actual < target_fps as f32 * 0.85 {
        Some(format!(
            "Recording performance is below target (actual ~{actual:.0} vs {target_fps} FPS). Consider lowering quality or FPS."
        ))
    } else {
        None
    }
}

fn live_frames(segs: &[LiveSeg]) -> u64 {
    segs.iter()
        .map(|s| s.session.frames.load(Ordering::SeqCst))
        .sum()
}

fn parts_bytes(parts: &[SegSet]) -> u64 {
    parts
        .iter()
        .flat_map(|s| s.files.iter())
        .filter_map(|(_, p)| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum()
}

fn status_of(rec: &Recorder) -> RecStatus {
    if let Some(r) = &rec.rec {
        let mut elapsed_ms = r.active_ms;
        if let Some(t) = r.active_start {
            elapsed_ms += t.elapsed().as_millis() as u64;
        }
        let elapsed_s = elapsed_ms / 1000;
        let frames = r.base_frames + live_frames(&r.segs);
        // Honest wall-clock shortfall (expected - delivered). `actual` is
        // MEASURED frames/second â€” never a metadata tag.
        let dropped = (elapsed_s * r.params.fps as u64).saturating_sub(frames);
        let (sys_db, mic_db) = match &r.pump {
            Some(p) => (p.sys_db(), p.mic_db()),
            None => (None, None),
        };
        let actual = if elapsed_s > 0 {
            frames as f32 / elapsed_s as f32
        } else {
            0.0
        };
        // Current part sizes (cheap metadata reads on the 1s tick).
        let mut size_bytes = parts_bytes(&r.parts);
        for s in &r.segs {
            size_bytes += std::fs::metadata(&s.path).map(|m| m.len()).unwrap_or(0);
        }
        return RecStatus {
            recording: !r.segs.is_empty(),
            paused: r.segs.is_empty(),
            replay: rec.replay_state().to_string(),
            replay_ready_s: rec
                .replay
                .as_ref()
                .map(|rp| rp.started_at.elapsed().as_secs())
                .unwrap_or(0),
            replay_duration: rec.replay.as_ref().map(|rp| rp.duration_s).unwrap_or(0),
            elapsed_s,
            frames,
            fps: r.params.fps,
            actual_fps: actual,
            width: r.params.out_w,
            height: r.params.out_h,
            encoder: r.params.codec.to_uppercase(),
            encoder_label: r.params.encoder_label.clone(),
            capture_api: NATIVE_CAPTURE_API.to_string(),
            size_bytes,
            dropped,
            duplicated: 0,
            sys_db,
            mic_db,
            audio_status: audio_status_text(&r.params.audio, sys_db, mic_db),
            perf_warning: perf_warning(r.params.fps, actual, elapsed_s),
            message: rec.last_message.clone(),
            ffmpeg: true,
            ffmpeg_path: None,
            power_note: r.params.power_note.clone(),
        };
    }
    if let Some(rp) = &rec.replay {
        let (sys_db, mic_db) = match &rp.pump {
            Some(p) => (p.sys_db(), p.mic_db()),
            None => (None, None),
        };
        let frames = live_frames(&rp.segs);
        let ready_s = rp.started_at.elapsed().as_secs();
        let actual = if ready_s > 0 {
            frames as f32 / ready_s as f32
        } else {
            0.0
        };
        return RecStatus {
            recording: false,
            paused: false,
            replay: rec.replay_state().to_string(),
            replay_ready_s: ready_s,
            replay_duration: rp.duration_s,
            elapsed_s: 0,
            frames,
            fps: rp.params.fps,
            actual_fps: actual,
            width: rp.params.out_w,
            height: rp.params.out_h,
            encoder: rp.params.codec.to_uppercase(),
            encoder_label: rp.params.encoder_label.clone(),
            capture_api: NATIVE_CAPTURE_API.to_string(),
            size_bytes: parts_bytes(&rp.closed),
            dropped: (ready_s * rp.params.fps as u64).saturating_sub(frames),
            duplicated: 0,
            sys_db,
            mic_db,
            audio_status: audio_status_text(&rp.params.audio, sys_db, mic_db),
            perf_warning: None,
            message: rec.last_message.clone(),
            ffmpeg: true,
            ffmpeg_path: None,
            power_note: None,
        };
    }
    RecStatus {
        recording: false,
        paused: false,
        replay: "off".to_string(),
        replay_ready_s: 0,
        replay_duration: 0,
        elapsed_s: 0,
        frames: 0,
        fps: 0,
        actual_fps: 0.0,
        width: 0,
        height: 0,
        encoder: String::new(),
        encoder_label: String::new(),
        capture_api: String::new(),
        size_bytes: 0,
        dropped: 0,
        duplicated: 0,
        sys_db: None,
        mic_db: None,
        audio_status: "idle".to_string(),
        perf_warning: None,
        message: rec.last_message.clone(),
        ffmpeg: true,
        ffmpeg_path: None,
        power_note: None,
    }
}

// ---------------------------------------------------------------------------
// Native sessions (WGC + Media Foundation). No subprocesses, no pipes, no
// progress scraping: frames, audio and stats flow through Rust types.
// ---------------------------------------------------------------------------





#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub container_fps: f32,
    pub video_samples: u64,
    pub codec: String,
    pub audio_codec: String,
    pub sample_rate: u32,
    pub duration_sec: f64,
}

/// REAL file verification from the container itself (pure-Rust MP4 parser â€”
/// no external prober). Reads dims, container fps (samples/duration),
/// codecs and duration: never the requested settings.
pub fn verify_media(path: &Path) -> MediaInfo {
    let mut info = MediaInfo::default();
    let parsed = match crate::mp4info::read_info(path) {
        Ok(i) => i,
        Err(_) => return info,
    };
    info.duration_sec = parsed.duration_sec;
    if let Some(v) = parsed.video.as_ref() {
        info.width = v.width;
        info.height = v.height;
        info.video_samples = v.samples as u64;
        info.codec = v.codec.clone();
        info.fps = crate::mp4info::container_fps(&parsed) as f32;
        info.container_fps = info.fps;
    }
    if let Some(a) = parsed.audio {
        info.audio_codec = a.codec;
        info.sample_rate = a.timescale;
    }
    info
}

// ---------------------------------------------------------------------------
// Disk space (Windows)
// ---------------------------------------------------------------------------

fn drive_free_bytes(path: &Path) -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        let s = path.to_string_lossy();
        let letter = s.chars().next()?;
        if !letter.is_ascii_alphabetic() {
            return None;
        }
        let out = crate::procutil::cmd("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-PSDrive {}).Free", letter),
            ])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        None
    }
}

fn check_disk(path: &Path, need_bytes: u64) -> Result<(), String> {
    if let Some(free) = drive_free_bytes(path) {
        if free < need_bytes {
            return Err("Not enough disk space to save this recording. Free up space and try again. / Ù„Ø§ ØªÙˆØ¬Ø¯ Ù…Ø³Ø§Ø­Ø© ÙƒØ§ÙÙŠØ© Ù„Ø­ÙØ¸ Ø§Ù„ØªØ³Ø¬ÙŠÙ„.".to_string());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recording history (metadata only â€” videos never loaded to RAM)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecHistoryItem {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub duration_s: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub size: u64,
    pub kind: String,
}

fn rec_history_path() -> PathBuf {
    settings::app_dir().join("recordings.json")
}

fn load_rec_history() -> Vec<RecHistoryItem> {
    std::fs::read(rec_history_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_rec_history(items: &[RecHistoryItem]) {
    if let Ok(b) = serde_json::to_vec_pretty(items) {
        let _ = std::fs::create_dir_all(settings::app_dir());
        let _ = std::fs::write(rec_history_path(), b);
    }
}

pub fn rec_history_list() -> Vec<RecHistoryItem> {
    let mut items = load_rec_history();
    // Prune files that no longer exist.
    items.retain(|i| Path::new(&i.path).exists());
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    items
}

pub fn rec_history_add(item: RecHistoryItem, limit: usize) {
    let mut items = load_rec_history();
    items.push(item);
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let lim = limit.clamp(0, 100);
    if lim == 0 {
        save_rec_history(&[]);
        return;
    }
    while items.len() > lim {
        if let Some(old) = items.pop() {
            let _ = std::fs::remove_file(&old.path);
        }
    }
    save_rec_history(&items);
}

pub fn rec_history_delete(id: &str) -> Result<(), String> {
    let mut items = load_rec_history();
    if let Some(pos) = items.iter().position(|i| i.id == id) {
        let item = items.remove(pos);
        let _ = std::fs::remove_file(&item.path);
        save_rec_history(&items);
    }
    Ok(())
}

pub fn rec_history_clear() -> Result<(), String> {
    for i in load_rec_history() {
        let _ = std::fs::remove_file(&i.path);
    }
    save_rec_history(&[]);
    Ok(())
}

// ---------------------------------------------------------------------------
// File naming
// ---------------------------------------------------------------------------

fn final_name(replay: bool) -> String {
    let now = chrono::Local::now();
    let stamp = format!(
        "{}_{}",
        now.format("%Y-%m-%d"),
        now.format("%H-%M-%S")
    );
    if replay {
        format!("OpenScreen_Replay_{stamp}.mp4")
    } else {
        format!("OpenScreen_{stamp}.mp4")
    }
}

// ---------------------------------------------------------------------------
// Recording operations (called with the shared lock + AppHandle)
// ---------------------------------------------------------------------------

fn default_folder(s: &AppSettings) -> PathBuf {
    let p = PathBuf::from(s.rec_folder.trim());
    if p.to_string_lossy().trim().is_empty() {
        dirs::video_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Open Screen")
    } else {
        p
    }
}

fn temp_workdir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&d);
    d
}

fn hw_mode_setting() -> String {
    match crate::settings::load_settings().rec_hw_mode.as_str() {
        "hw" => "hw".to_string(),
        "sw" => "sw".to_string(),
        _ => "auto".to_string(),
    }
}

/// One monitor's share of a recording: full monitor or a clamped crop box
/// (monitor-relative physical pixels) with aspect-fit output (never upscale).
struct NativeTarget {
    monitor_idx: usize,
    crop: Option<(u32, u32, u32, u32)>,
    out_w: u32,
    out_h: u32,
}

fn resolve_targets(area: &Area, out_w: u32, out_h: u32) -> Result<Vec<NativeTarget>, String> {
    let mons = crate::screenshot::list_monitors().map_err(|e| e.to_string())?;
    if mons.is_empty() {
        return Err("No monitors found".to_string());
    }
    let all = is_full_virtual(area);
    let mut out = vec![];
    for (i, m) in mons.iter().enumerate() {
        let ix0 = area.x.max(m.x);
        let iy0 = area.y.max(m.y);
        let ix1 = (area.x + area.w as i32).min(m.x + m.width as i32);
        let iy1 = (area.y + area.h as i32).min(m.y + m.height as i32);
        if ix1 <= ix0 || iy1 <= iy0 {
            continue;
        }
        let mw = m.width;
        let mh = m.height;
        if all || (ix0 == m.x && iy0 == m.y && (ix1 - ix0) as u32 == mw && (iy1 - iy0) as u32 == mh) {
            // Whole monitor at source geometry â€” zero-copy DirectX path.
            out.push(NativeTarget { monitor_idx: i, crop: None, out_w: mw, out_h: mh });
            continue;
        }
        let cw = (ix1 - ix0) as u32;
        let ch = (iy1 - iy0) as u32;
        let k = ((out_w as f64 / cw.max(1) as f64).min(out_h as f64 / ch.max(1) as f64)).min(1.0);
        let ow = even((cw as f64 * k).round() as u32).max(64);
        let oh = even((ch as f64 * k).round() as u32).max(64);
        out.push(NativeTarget {
            monitor_idx: i,
            crop: Some(((ix0 - m.x) as u32, (iy0 - m.y) as u32, (ix1 - m.x) as u32, (iy1 - m.y) as u32)),
            out_w: ow,
            out_h: oh,
        });
    }
    if out.is_empty() {
        return Err("Selected area is outside all monitors".to_string());
    }
    Ok(out)
}

fn native_cfg(params: &Params, t: &NativeTarget) -> crate::nativerec::NativeRecConfig {
    crate::nativerec::NativeRecConfig {
        monitor_idx: t.monitor_idx,
        crop: t.crop,
        out_w: t.out_w,
        out_h: t.out_h,
        fps: params.fps,
        bitrate_bps: params.bitrate_bps,
        codec: if params.codec == "hevc" {
            crate::nativerec::NativeCodec::Hevc
        } else {
            crate::nativerec::NativeCodec::H264
        },
        cursor: params.cursor,
        audio_channels: if params.audio == "none" { 0 } else { params.channels },
        sample_rate: params.sample_rate,
    }
}

struct FinishedSeg {
    monitor_idx: usize,
    path: PathBuf,
    frames: u64,
    audio_bytes: u64,
    error: Option<String>,
}

/// Blocking: stop sessions, finalize their MP4s, report encoder-confirmed
/// frame counts + pushed audio bytes. Always call via `spawn_blocking`
/// (joins OS threads).
fn finish_segs(segs: Vec<LiveSeg>) -> Vec<FinishedSeg> {
    segs.into_iter()
        .map(|s| {
            let approx = s.session.frames.load(Ordering::SeqCst);
            let err_arc = s.session.error.clone();
            match s.session.stop_and_join() {
                Ok(st) => FinishedSeg {
                    monitor_idx: s.monitor_idx,
                    path: s.path,
                    frames: st.frames,
                    audio_bytes: st.audio_bytes,
                    error: err_arc.lock().ok().and_then(|mut e| e.take()),
                },
                Err(e) => FinishedSeg {
                    monitor_idx: s.monitor_idx,
                    path: s.path,
                    frames: approx,
                    audio_bytes: 0,
                    error: Some(e),
                },
            }
        })
        .collect()
}

/// A finished part file worth merging (drops empty/corrupt stubs from
/// instant pauses so the remuxer never chokes).
fn valid_part(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 4096).unwrap_or(false)
}

impl Recorder {
    fn note(&mut self, m: &str) {
        self.last_message = m.to_string();
    }

    fn stop_timer(&mut self) {
        self.timer_gen.fetch_add(1, Ordering::SeqCst);
        if let Some(h) = self.timer_handle.take() {
            h.abort();
        }
    }

    fn start_timer(&mut self, shared: &Shared, app: &tauri::AppHandle) {
        self.stop_timer();
        let gen = self.timer_gen.clone();
        let cur = gen.load(Ordering::SeqCst);
        let shared_c = shared.clone();
        let app_h = app.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if gen.load(Ordering::SeqCst) != cur {
                    break;
                }
                let (tip, active) = {
                    let mut rec = shared_c.lock().await;
                    // Watchdog: a native session thread died on its own
                    // (monitor unplugged, driver crash, encoder blew up).
                    // Say so NOW â€” a silent truncated file is the worst UX.
                    // Sessions that the user stopped/paused are already gone
                    // from `segs`, so anything finished here is unexpected.
                    let mut died: Option<String> = None;
                    if let Some(r) = rec.rec.as_mut() {
                        if !r.watch_noted {
                            for s in &r.segs {
                                if s.session.is_finished() {
                                    if let Some(e) = s.session.take_error() {
                                        r.watch_noted = true;
                                        died = Some(format!("Screen capture stopped unexpectedly ({e}). Press Stop to finalize what was recorded."));
                                        break;
                                    }
                                    r.watch_noted = true;
                                    died = Some("Screen capture stopped unexpectedly. Press Stop to finalize what was recorded.".to_string());
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(rp) = rec.replay.as_mut() {
                        if !rp.watch_noted {
                            for s in &rp.segs {
                                if s.session.is_finished() {
                                    rp.watch_noted = true;
                                    died = Some("Replay buffer stopped unexpectedly. Restart Instant Replay.".to_string());
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(m) = died {
                        rec.last_message = m;
                        crate::rebuild_tray(&app_h, &status_of(&rec));
                    }
                    (tooltip_text(&rec), rec.rec.is_some() || rec.replay.is_some())
                };
                if !active {
                    break;
                }
                if let Some(tray) = app_h.tray_by_id("main") {
                    let _ = tray.set_tooltip(Some(tip.as_str()));
                }
            }
        });
        self.timer_handle = Some(handle.abort_handle());
    }

    /// Start a normal recording of `area` (already normalized virtual coords).
    /// Native pipeline: WGC sessions (one per monitor) + WASAPI pump, all
    /// started in the same instant; smooth start absorbs first-frame warmup.
    pub async fn start_recording(
        &mut self,
        shared: Shared,
        app: &tauri::AppHandle,
        area: Area,
    ) -> Result<RecStatus, String> {
        if self.rec.is_some() {
            return Err("Already recording.".to_string());
        }
        let s = settings::load_settings();
        let folder = default_folder(&s);
        std::fs::create_dir_all(&folder).map_err(|_| {
            "Unable to create the recording folder. Choose another folder in Settings. / ØªØ¹Ø°Ù‘Ø± Ø¥Ù†Ø´Ø§Ø¡ Ù…Ø¬Ù„Ø¯ Ø§Ù„ØªØ³Ø¬ÙŠÙ„.".to_string()
        })?;
        check_disk(&folder, 1024 * 1024 * 1024)?; // 1 GB for open-ended recording
        let probe = get_probe(false)?;
        let params = resolve_params(&s, &probe, area.clone(), false)?;
        if let Some(n) = &params.codec_note {
            self.note(n);
        }
        if let Some(n) = &params.audio_note {
            if self.last_message.is_empty() {
                self.note(n);
            }
        }
        if !params.hw && s.rec_hw_mode != "sw" && self.last_message.is_empty() {
            self.note("Hardware encoder unavailable. Using software encoding.");
        }
        let targets = resolve_targets(&params.area, params.out_w, params.out_h)?;
        let workdir = temp_workdir("openscreen-rec");
        dbg_pipe!(
            "native rec backend={} area={}x{}@{},{} out={}x{}@{}fps codec={} ({}) bitrate={}k audio={} mic={:?} cursor={} targets={}",
            NATIVE_CAPTURE_API,
            params.area.w,
            params.area.h,
            params.area.x,
            params.area.y,
            params.out_w,
            params.out_h,
            params.fps,
            params.codec,
            params.encoder_label,
            params.bitrate_k,
            params.audio,
            params.mic,
            params.cursor,
            targets.len(),
        );
        // Video sessions first (fast-fail before touching audio devices).
        let mut segs = vec![];
        for (i, t) in targets.iter().enumerate() {
            let part = workdir.join(format!("part{:03}.mp4", i + 1));
            let cfg = native_cfg(&params, t);
            match crate::nativerec::start_session(cfg, part.clone()) {
                Ok(session) => segs.push(LiveSeg {
                    session,
                    path: part,
                    monitor_idx: t.monitor_idx,
                }),
                Err(e) => {
                    // Tear down siblings; never leave half a recording.
                    for s in segs.drain(..) {
                        let _ = s.session.stop_and_join();
                        let _ = std::fs::remove_file(&s.path);
                    }
                    let _ = std::fs::remove_dir_all(&workdir);
                    self.note(&e);
                    return Err(e);
                }
            }
        }
        // Audio pump fans out to every live session. Degraded audio never
        // fails video â€” note once and continue silent.
        let txs: Vec<_> = segs.iter().map(|sg| sg.session.audio_tx.clone()).collect();
        let (pump, audio_note) = crate::audio::start_pcm_pump(
            &params.audio,
            params.mic.as_deref().unwrap_or(""),
            params.sample_rate,
            params.channels,
            txs,
        );
        if let Some(n) = audio_note {
            if self.last_message.is_empty() {
                self.note(&n);
            }
        }
        let pump_opt = if pump.is_active() { Some(pump) } else { None };
        self.rec = Some(ActiveRec {
            segs,
            parts: vec![],
            pump: pump_opt,
            workdir,
            active_ms: 0,
            active_start: Some(Instant::now()),
            params: params.clone(),
            final_name: final_name(false),
            base_frames: 0,
            final_frames: 0,
            watch_noted: false,
        });
        self.start_timer(&shared, app);
        self.apply_tray(app);
        self.notify(
            app,
            "Open Screen â€” Recording",
            "Recording started. Use the tray menu or Ctrl+Shift+R to stop.",
        );
        Ok(status_of(self))
    }

    pub async fn pause_recording(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        // Take live sessions out, finalize them off-thread, seal as a part.
        let (segs, pump, workdir, params) = match self.rec.as_mut() {
            Some(r) if !r.segs.is_empty() => (
                std::mem::take(&mut r.segs),
                r.pump.take(),
                r.workdir.clone(),
                r.params.clone(),
            ),
            Some(_) => return Err("Already paused.".to_string()),
            None => return Err("Not recording.".to_string()),
        };
        let finished: Vec<FinishedSeg> =
            tokio::task::spawn_blocking(move || finish_segs(segs))
                .await
                .map_err(|e| e.to_string())?;
        if let Some(p) = pump {
            p.stop_and_join();
        }
        let mut files = vec![];
        let mut frames = 0u64;
        for f in finished {
            frames += f.frames;
            if valid_part(&f.path) {
                files.push((f.monitor_idx, f.path));
            } else {
                let _ = std::fs::remove_file(&f.path);
            }
        }
        let _ = (workdir, params);
        if let Some(r) = self.rec.as_mut() {
            if !files.is_empty() {
                r.parts.push(SegSet {
                    files,
                    finished_at: std::time::SystemTime::now(),
                });
            }
            r.base_frames += frames;
            if let Some(t) = r.active_start.take() {
                r.active_ms += t.elapsed().as_millis() as u64;
            }
        }
        self.apply_tray(app);
        Ok(status_of(self))
    }

    pub async fn resume_recording(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let (params, workdir, part_count) = match &self.rec {
            Some(r) if r.segs.is_empty() => (
                r.params.clone(),
                r.workdir.clone(),
                r.parts.iter().map(|s| s.files.len()).sum::<usize>(),
            ),
            Some(_) => return Err("Already recording.".to_string()),
            None => return Err("Not recording.".to_string()),
        };
        let targets = resolve_targets(&params.area, params.out_w, params.out_h)?;
        let mut segs = vec![];
        for (i, t) in targets.iter().enumerate() {
            let part = workdir.join(format!("resume{:03}_{i}.mp4", part_count + 1));
            let cfg = native_cfg(&params, t);
            match crate::nativerec::start_session(cfg, part.clone()) {
                Ok(session) => segs.push(LiveSeg { session, path: part, monitor_idx: t.monitor_idx }),
                Err(e) => {
                    for s in segs.drain(..) {
                        let _ = s.session.stop_and_join();
                        let _ = std::fs::remove_file(&s.path);
                    }
                    self.note(&e);
                    return Err(e);
                }
            }
        }
        let txs: Vec<_> = segs.iter().map(|sg| sg.session.audio_tx.clone()).collect();
        let (pump, _) = crate::audio::start_pcm_pump(
            &params.audio,
            params.mic.as_deref().unwrap_or(""),
            params.sample_rate,
            params.channels,
            txs,
        );
        let pump_opt = if pump.is_active() { Some(pump) } else { None };
        if let Some(r) = self.rec.as_mut() {
            r.segs = segs;
            r.pump = pump_opt;
            r.active_start = Some(Instant::now());
        }
        self.apply_tray(app);
        Ok(status_of(self))
    }

    /// Stop recording: finish live segments (blocking finalize off-thread),
    /// merge parts per monitor with stream copy, verify the real file,
    /// history, post-action. Smooth stop order: video flush -> audio flush
    /// -> remux -> verify -> save. Nothing recorded is ever deleted silently.
    pub async fn stop_recording(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let mut r = self.rec.take().ok_or("Not recording.".to_string())?;
        if let Some(t) = r.active_start.take() {
            r.active_ms += t.elapsed().as_millis() as u64;
        }
        // 1) Finish live sessions (MP4 finalize happens here).
        let live = std::mem::take(&mut r.segs);
        let finished: Vec<FinishedSeg> = tokio::task::spawn_blocking(move || finish_segs(live))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(pump) = r.pump.take() {
            pump.stop_and_join();
        }
        let mut all_parts = r.parts;
        if !finished.is_empty() {
            let mut files = vec![];
            let mut frames = 0u64;
            let mut audio_bytes = 0u64;
            for f in finished {
                frames += f.frames;
                audio_bytes += f.audio_bytes;
                if let Some(e) = f.error {
                    self.note(&format!("A segment ended early: {e}"));
                }
                if valid_part(&f.path) {
                    files.push((f.monitor_idx, f.path));
                } else {
                    let _ = std::fs::remove_file(&f.path);
                }
            }
            r.base_frames += frames;
            // Honest audio claim (#67): configured but silent => say so.
            if r.params.audio != "none" && audio_bytes == 0 {
                self.note("No audio signal was captured â€” check the input device and volume.");
            }
            if !files.is_empty() {
                all_parts.push(SegSet { files, finished_at: std::time::SystemTime::now() });
            }
        }
        r.final_frames = r.base_frames;
        if let Some(t) = r.active_start.take() {
            r.active_ms += t.elapsed().as_millis() as u64;
        }
        // Group finished parts per monitor, preserving order.
        let mut per_mon: std::collections::BTreeMap<usize, Vec<PathBuf>> = Default::default();
        for set in &all_parts {
            for (mi, p) in &set.files {
                per_mon.entry(*mi).or_default().push(p.clone());
            }
        }
        per_mon.retain(|_, v| {
            v.retain(|p| valid_part(p));
            !v.is_empty()
        });
        if per_mon.is_empty() {
            let _ = std::fs::remove_dir_all(&r.workdir);
            self.stop_timer();
            self.note("Nothing was recorded.");
            self.apply_tray(app);
            return Ok(status_of(self));
        }
        let s = settings::load_settings();
        let folder = default_folder(&s);
        let _ = std::fs::create_dir_all(&folder);
        let temp_size: u64 = per_mon
            .values()
            .flatten()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        check_disk(&folder, temp_size + 100 * 1024 * 1024)?;
        // 2) Merge per monitor (stream copy, one timeline each). Multi-monitor
        // recordings produce one honest file per screen.
        let multi = per_mon.len() > 1;
        let mut outs: Vec<(usize, PathBuf)> = vec![];
        for (mi, files) in &per_mon {
            let name = if multi {
                let stem = r.final_name.trim_end_matches(".mp4");
                format!("{stem}_S{}.mp4", mi + 1)
            } else {
                r.final_name.clone()
            };
            let out = folder.join(&name);
            let workdir_str = r.workdir.to_string_lossy().to_string();
            crate::mp4mux::remux_segments(files, &out).map_err(|e| {
                self.note(&format!("Finalize failed â€” parts kept at {workdir_str}. {e}"));
                e
            })?;
            outs.push((*mi, out));
        }
        let _ = std::fs::remove_dir_all(&r.workdir);
        let elapsed_ms = r.active_ms;
        // 3) REAL verification from the container (never the requested
        // settings): dims, container fps, codecs, duration + duplicate math
        // (container samples minus pushed frames).
        for (mi, out) in &outs {
            let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
            let (hist_w, hist_h, hist_fps, dups) = match crate::mp4info::read_info(out) {
                Ok(info) => {
                    let cfps = crate::mp4info::container_fps(&info).round() as u32;
                    let (w, h, samples) = match info.video.as_ref() {
                        Some(t) => (t.width, t.height, t.samples as u64),
                        None => (r.params.out_w, r.params.out_h, 0),
                    };
                    let d = samples.saturating_sub(r.final_frames);
                    if w != r.params.out_w || h != r.params.out_h {
                        self.note(&format!("Saved at {w}x{h} (target {}x{}).", r.params.out_w, r.params.out_h));
                    } else if (cfps as i32 - r.params.fps as i32).abs() > 5 {
                        self.note(&format!(
                            "Actual ~{cfps} FPS (target {} FPS). See Advanced performance.",
                            r.params.fps
                        ));
                    }
                    let _ = mi;
                    (w, h, cfps, d)
                }
                Err(e) => {
                    self.note(&format!("Saved file failed verification ({e})."));
                    (r.params.out_w, r.params.out_h, r.params.fps, 0)
                }
            };
            let _ = dups;
            let name = out.file_name().and_then(|n| n.to_str()).unwrap_or(&r.final_name).to_string();
            rec_history_add(
                RecHistoryItem {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    path: out.to_string_lossy().to_string(),
                    created_at: chrono::Local::now().to_rfc3339(),
                    duration_s: elapsed_ms / 1000,
                    width: hist_w,
                    height: hist_h,
                    fps: hist_fps,
                    size,
                    kind: "screen".to_string(),
                },
                s.rec_history_limit,
            );
        }
        self.stop_timer();
        self.apply_tray(app);
        self.notify(app, "Open Screen â€” Recording saved", &r.final_name);
        let first_out = outs.first().map(|(_, p)| p.clone()).unwrap_or_else(|| folder.join(&r.final_name));
        apply_post_action(app, &s.rec_post_action, &first_out);
        if self.last_message.is_empty() {
            self.note("");
        }
        Ok(status_of(self))
    }

    // -------------------------------------------------- Instant Replay ----
    // Native rolling buffer: sequential 10s MP4 segments (each starts with a
    // keyframe) + ONE gapless WASAPI pump for the whole buffer lifetime.
    // Save = rotate once (buffer keeps rolling) + stream-copy remux of the
    // last N seconds. No countdown on save, no re-encode, no ffmpeg.
    pub async fn replay_start(
        &mut self,
        shared: Shared,
        app: &tauri::AppHandle,
    ) -> Result<RecStatus, String> {
        if self.replay.is_some() {
            return Err("Instant Replay is already on.".to_string());
        }
        let s = settings::load_settings();
        let probe = get_probe(false)?;
        // Replay captures the full virtual screen (every monitor).
        let area = full_virtual_area();
        let params = resolve_params(&s, &probe, area, true)?;
        if let Some(n) = &params.codec_note {
            self.note(n);
        }
        if let Some(n) = &params.audio_note {
            if self.last_message.is_empty() {
                self.note(n);
            }
        }
        let segdir = std::env::temp_dir().join("openscreen-replay");
        let _ = std::fs::remove_dir_all(&segdir);
        std::fs::create_dir_all(&segdir).map_err(|e| e.to_string())?;
        check_disk(&segdir, 512 * 1024 * 1024)?;
        let duration = s.replay_duration.clamp(5, 600);
        let targets = resolve_targets(&params.area, params.out_w, params.out_h)?;
        let (segs, pump) = Self::start_seg_set(&params, &targets, &segdir, 0, None, &mut self.last_message)?;
        self.replay = Some(ActiveReplay {
            segs,
            pump,
            closed: vec![],
            segdir,
            started_at: Instant::now(),
            duration_s: duration,
            params,
            supervisor: Some(Self::spawn_rotator(shared.clone())),
            watch_noted: false,
        });
        self.apply_tray(app);
        self.notify(
            app,
            "Instant Replay",
            &format!("Buffering last {duration}s. Press Ctrl+Shift+I to save."),
        );
        Ok(status_of(self))
    }

    /// Start one segment set across monitors + wire the shared audio pump.
    /// `idx` numbers the files. `pump` is reused across rotations (gapless
    /// audio) and only created when absent. Associated (not `&mut self`) so
    /// rotation/supervisor code can run alongside a borrowed recorder.
    fn start_seg_set(
        params: &Params,
        targets: &[NativeTarget],
        segdir: &Path,
        idx: usize,
        pump: Option<crate::audio::PcmPump>,
        note_sink: &mut String,
    ) -> Result<(Vec<LiveSeg>, Option<crate::audio::PcmPump>), String> {
        let mut segs = vec![];
        for (i, t) in targets.iter().enumerate() {
            let part = segdir.join(format!("seg{idx:05}_m{i}.mp4"));
            let cfg = native_cfg(params, t);
            match crate::nativerec::start_session(cfg, part.clone()) {
                Ok(session) => segs.push(LiveSeg { session, path: part, monitor_idx: t.monitor_idx }),
                Err(e) => {
                    for s in segs.drain(..) {
                        let _ = s.session.stop_and_join();
                        let _ = std::fs::remove_file(&s.path);
                    }
                    return Err(e);
                }
            }
        }
        let txs: Vec<_> = segs.iter().map(|sg| sg.session.audio_tx.clone()).collect();
        let pump_opt = match pump {
            Some(p) => {
                p.retarget(txs);
                Some(p)
            }
            None => {
                let (pump, note) = crate::audio::start_pcm_pump(
                    &params.audio,
                    params.mic.as_deref().unwrap_or(""),
                    params.sample_rate,
                    params.channels,
                    txs,
                );
                if let Some(n) = note {
                    if note_sink.is_empty() {
                        *note_sink = n;
                    }
                }
                if pump.is_active() { Some(pump) } else { None }
            }
        };
        Ok((segs, pump_opt))
    }

    /// Rotation supervisor: every SEG_SECS finishes the open set (blocking
    /// finalize off-thread), seals it, opens a fresh set on the SAME pump
    /// (audio never gaps), and prunes sets older than duration + margin.
    /// Pure rolling buffer â€” nothing is ever a "final file" until save.
    fn spawn_rotator(shared: Shared) -> tokio::task::JoinHandle<()> {
        const SEG_SECS: u64 = 10;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(SEG_SECS)).await;
                // Resolve FIRST while the old set keeps recording: a display
                // change (#76) skips the tick with zero interruption.
                let staged = {
                    let mut rec = shared.lock().await;
                    let Some(rp) = rec.replay.as_mut() else { break };
                    let params = rp.params.clone();
                    match resolve_targets(&params.area, params.out_w, params.out_h) {
                        Ok(t) if !t.is_empty() => {
                            let segs = std::mem::take(&mut rp.segs);
                            let segdir = rp.segdir.clone();
                            let idx = rp.closed.len() + 100;
                            Some((params, segdir, idx, segs))
                        }
                        Ok(_) => {
                            rec.note("Display configuration changed â€” replay retrying.");
                            None
                        }
                        Err(e) => {
                            rec.note(&format!("Display change ignored for replay ({e}). Retrying."));
                            None
                        }
                    }
                };
                let Some((params, segdir, idx, segs)) = staged else { continue };
                let finished: Vec<FinishedSeg> =
                    tokio::task::spawn_blocking(move || finish_segs(segs))
                        .await
                        .unwrap_or_default();
                let mut rec = shared.lock().await;
                let Some(rp) = rec.replay.as_mut() else { break };
                let mut files = vec![];
                for f in finished {
                    if valid_part(&f.path) {
                        files.push((f.monitor_idx, f.path));
                    } else {
                        let _ = std::fs::remove_file(&f.path);
                    }
                }
                if !files.is_empty() {
                    rp.closed.push(SegSet { files, finished_at: std::time::SystemTime::now() });
                }
                // Fresh set on the live pump (audio gapless via retarget).
                // Targets were resolved BEFORE the old set was touched, so a
                // start failure here only affects this tick (old footage is
                // already sealed in `closed`).
                let targets = resolve_targets(&params.area, params.out_w, params.out_h)
                    .unwrap_or_default();
                let mut rec = shared.lock().await;
                let Some(rp) = rec.replay.as_mut() else { break };
                if targets.is_empty() {
                    rec.note("Display configuration changed â€” replay retrying.");
                    drop(rec);
                    continue;
                }
                let pump = rp.pump.take();
                // Release the guard before the blocking session starts
                // (WGC init can take a moment; never hold the lock for it).
                let mut last_message = std::mem::take(&mut rec.last_message);
                drop(rec);
                let started = Self::start_seg_set(&params, &targets, &segdir, idx, pump, &mut last_message);
                let mut rec = shared.lock().await;
                rec.last_message = last_message;
                match started {
                    Ok((segs, pump)) => match rec.replay.as_mut() {
                        Some(rp) => {
                            rp.segs = segs;
                            rp.pump = pump;
                        }
                        None => {
                            // Replay stopped mid-rotation: finalize + delete
                            // orphans off-thread (never leak threads/files).
                            drop(rec);
                            tokio::task::spawn_blocking(move || {
                                for s in segs {
                                    let _ = s.session.stop_and_join();
                                    let _ = std::fs::remove_file(&s.path);
                                }
                                if let Some(p) = pump {
                                    p.stop_and_join();
                                }
                            })
                            .await
                            .ok();
                            break;
                        }
                    },
                    Err(e) => {
                        rec.note(&format!("Replay rotation failed ({e}). Retrying."));
                    }
                }
                // Prune sets fully older than duration + 25s margin.
                let Some(rp) = rec.replay.as_mut() else { break };
                let cutoff = std::time::SystemTime::now()
                    .checked_sub(Duration::from_secs(rp.duration_s + 25))
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                rp.closed.retain(|s| {
                    let keep = s.finished_at >= cutoff;
                    if !keep {
                        for (_, p) in &s.files {
                            let _ = std::fs::remove_file(p);
                        }
                    }
                    keep
                });
            }
        })
    }

    pub async fn replay_stop(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let mut rp = self.replay.take().ok_or("Instant Replay is off.".to_string())?;
        if let Some(h) = rp.supervisor.take() {
            h.abort();
        }
        let segs = std::mem::take(&mut rp.segs);
        let finished: Vec<FinishedSeg> =
            tokio::task::spawn_blocking(move || finish_segs(segs))
                .await
                .map_err(|e| e.to_string())?;
        for f in finished {
            let _ = std::fs::remove_file(&f.path);
        }
        if let Some(pump) = rp.pump.take() {
            pump.stop_and_join();
        }
        let _ = std::fs::remove_dir_all(&rp.segdir);
        self.apply_tray(app);
        Ok(status_of(self))
    }

    /// Save the last N seconds as final MP4(s). NO countdown: export is
    /// strictly T-N..T. The buffer is rotated first and keeps rolling
    /// throughout â€” saving never interrupts buffering.
    pub async fn replay_save(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let ready_s = self
            .replay
            .as_ref()
            .map(|rp| rp.started_at.elapsed().as_secs())
            .unwrap_or(0);
        if self.replay.is_none() {
            return Err("Instant Replay is off. Enable it first.".to_string());
        }
        if ready_s < 3 {
            return Err("Instant Replay is not ready yet. Please wait a few seconds. / Ø§Ù†ØªØ¸Ø± Ø¨Ø¶Ø¹ Ø«ÙˆØ§Ù†Ù Ø­ØªÙ‰ ÙŠØ¬Ù‡Ø².".to_string());
        }
        self.replay_saving = true;
        self.apply_tray(app);
        // Rotate-then-merge (buffer never stops; no countdown on save).
        let segdir = self
            .replay
            .as_ref()
            .map(|rp| rp.segdir.clone())
            .unwrap_or_else(|| std::env::temp_dir().join("openscreen-replay"));
        let duration_s = self.replay.as_ref().map(|rp| rp.duration_s).unwrap_or(30);
        let result = self.replay_rotate_and_save(app, &segdir, duration_s).await;
        self.replay_saving = false;
        match result {
            Ok(_) => {}
            Err(e) => {
                self.note(&e);
                self.apply_tray(app);
                return Err(e);
            }
        }
        self.apply_tray(app);
        Ok(status_of(self))
    }

    /// Rotate-then-merge for replay save. Returns after fresh segments are
    /// live again; the merged file(s) are already in the recording folder.
    async fn replay_rotate_and_save(
        &mut self,
        app: &tauri::AppHandle,
        segdir: &Path,
        duration_s: u64,
    ) -> Result<(), String> {
        // Finish open set off-thread.
        let (segs, params) = {
            let rp = self.replay.as_mut().ok_or("Instant Replay is off.".to_string())?;
            (std::mem::take(&mut rp.segs), rp.params.clone())
        };
        let finished: Vec<FinishedSeg> =
            tokio::task::spawn_blocking(move || finish_segs(segs))
                .await
                .map_err(|e| e.to_string())?;
        // Reopen immediately (rolling continues during the merge below).
        {
            let rp = self.replay.as_mut().ok_or("Instant Replay is off.".to_string())?;
            let pump = rp.pump.take();
            let targets = resolve_targets(&params.area, params.out_w, params.out_h)
                .map_err(|e| format!("Display changed during save: {e}"))?;
            let idx = rp.closed.len() + 2000;
            let mut sink = std::mem::take(&mut self.last_message);
            match Self::start_seg_set(&params, &targets, segdir, idx, pump, &mut sink) {
                Ok((segs, pump)) => {
                    rp.segs = segs;
                    rp.pump = pump;
                }
                Err(e) => {
                    // Buffer degraded but the sealed tail is still mergeable.
                    sink = format!("Replay buffer restart failed ({e}); merging sealed part.");
                }
            }
            if self.last_message.is_empty() {
                self.last_message = sink;
            }
        }
        // Seal the just-finished set into the window.
        let mut fresh: Vec<(usize, PathBuf)> = vec![];
        for f in finished {
            if valid_part(&f.path) {
                fresh.push((f.monitor_idx, f.path));
            }
        }
        {
            let rp = self.replay.as_mut().ok_or("Instant Replay is off.".to_string())?;
            if !fresh.is_empty() {
                rp.closed.push(SegSet { files: fresh, finished_at: std::time::SystemTime::now() });
            }
        }
        // 2) Merge per monitor over the closed window, trimmed to duration.
        let (closed, post_action) = {
            let rp = self.replay.as_ref().ok_or("Instant Replay is off.".to_string())?;
            (rp.closed.clone(), settings::load_settings().replay_post_action.clone())
        };
        let mut per_mon: std::collections::BTreeMap<usize, Vec<PathBuf>> = Default::default();
        for set in &closed {
            for (mi, p) in &set.files {
                if p.exists() {
                    per_mon.entry(*mi).or_default().push(p.clone());
                }
            }
        }
        if per_mon.is_empty() {
            return Err("Instant Replay is not ready yet. Please wait a few seconds.".to_string());
        }
        let s = settings::load_settings();
        let folder = default_folder(&s);
        let _ = std::fs::create_dir_all(&folder);
        let multi = per_mon.len() > 1;
        let base = final_name(true);
        let mut first_out = String::new();
        for (mi, files) in &per_mon {
            let name = if multi {
                format!("{}_S{}.mp4", base.trim_end_matches(".mp4"), mi + 1)
            } else {
                base.clone()
            };
            let out = folder.join(&name);
            crate::mp4mux::remux_segments_keep_last(files, &out, duration_s as f64).map_err(|e| {
                format!("Replay merge failed: {e}")
            })?;
            if first_out.is_empty() {
                first_out = out.to_string_lossy().to_string();
            }
            let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            let (hist_w, hist_h, hist_fps) = match crate::mp4info::read_info(&out) {
                Ok(info) => (
                    info.video.as_ref().map(|v| v.width).unwrap_or(params.out_w),
                    info.video.as_ref().map(|v| v.height).unwrap_or(params.out_h),
                    crate::mp4info::container_fps(&info).round() as u32,
                ),
                Err(_) => (params.out_w, params.out_h, params.fps),
            };
            rec_history_add(
                RecHistoryItem {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.clone(),
                    path: out.to_string_lossy().to_string(),
                    created_at: chrono::Local::now().to_rfc3339(),
                    duration_s,
                    width: hist_w,
                    height: hist_h,
                    fps: hist_fps,
                    size,
                    kind: "replay".to_string(),
                },
                s.rec_history_limit,
            );
        }
        self.notify(app, "Instant Replay saved", &first_out);
        apply_post_action(app, &post_action, Path::new(&first_out));
        Ok(())
    }

    // ------------------------------------------------------------ helpers --
    fn notify(&self, app: &tauri::AppHandle, title: &str, body: &str) {
        use tauri_plugin_notification::NotificationExt;
        let _ = app.notification().builder().title(title).body(body).show();
    }

    fn apply_tray(&mut self, app: &tauri::AppHandle) {
        // Build / swap the red-dot tray icon while recording.
        if self.rec.is_some() && self.rec_icon.is_none() {
            self.rec_icon = Some(make_rec_dot());
        }
        if let Some(tray) = app.tray_by_id("main") {
            if self.rec.is_some() {
                if let Some((rgba, w, h)) = &self.rec_icon {
                    let _ = tray.set_icon(Some(tauri::image::Image::new_owned(
                        rgba.clone(),
                        *w,
                        *h,
                    )));
                }
            } else {
                let _ = tray.set_icon(None);
            }
            let tip = tooltip_text(self);
            let _ = tray.set_tooltip(Some(tip.as_str()));
        }
        crate::rebuild_tray(app, &status_of(self));
    }
}

/// Tray tooltip text for the current state (timer ticks + instant refreshes).
/// Minimal live status: REC + time + res/fps + audio (no heavy UI).
fn tooltip_text(rec: &Recorder) -> String {
    if let Some(r) = &rec.rec {
        let mut ms = r.active_ms;
        if let Some(t) = r.active_start {
            ms += t.elapsed().as_millis() as u64;
        }
        let s = ms / 1000;
        let res = format!("{}p", r.params.out_h);
        let audio = match r.params.audio.as_str() {
            "both" => "System+Mic",
            "system" => "System",
            "mic" | "microphone" => "Mic",
            _ => "Muted",
        };
        if !r.segs.is_empty() {
            format!(
                "â— REC {:02}:{:02} Â· {} {} FPS Â· {} â€” Ctrl+Shift+R to stop",
                s / 60,
                s % 60,
                res,
                r.params.fps,
                audio
            )
        } else {
            format!("âšâš Paused {:02}:{:02} Â· {} {} FPS", s / 60, s % 60, res, r.params.fps)
        }
    } else if let Some(rp) = &rec.replay {
        format!(
            "Instant Replay: {} ({}s)",
            rec.replay_state(),
            rp.duration_s
        )
    } else {
        "Open Screen".to_string()
    }
}

/// Red-dot 32x32 tray icon generated at runtime (base icon + red circle).
fn make_rec_dot() -> (Vec<u8>, u32, u32) {
    let bytes = include_bytes!("../icons/32x32.png");
    let base = image::load_from_memory(bytes)
        .map(|i| i.to_rgba8())
        .unwrap_or_else(|_| image::RgbaImage::from_pixel(32, 32, image::Rgba([11, 87, 208, 255])));
    let (w, h) = (base.width(), base.height());
    let mut img = base;
    let cx = w as i32 - 9;
    let cy = h as i32 - 9;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let d = (x - cx) * (x - cx) + (y - cy) * (y - cy);
            if d <= 49 {
                img.put_pixel(x as u32, y as u32, image::Rgba([232, 17, 35, 255]));
            } else if d <= 81 {
                img.put_pixel(x as u32, y as u32, image::Rgba([255, 255, 255, 255]));
            }
        }
    }
    (img.into_raw(), w, h)
}

fn full_virtual_area() -> Area {
    match crate::screenshot::list_monitors() {
        Ok(ms) if !ms.is_empty() => {
            let min_x = ms.iter().map(|m| m.x).min().unwrap_or(0);
            let min_y = ms.iter().map(|m| m.y).min().unwrap_or(0);
            let max_x = ms.iter().map(|m| m.x + m.width as i32).max().unwrap_or(0);
            let max_y = ms.iter().map(|m| m.y + m.height as i32).max().unwrap_or(0);
            Area {
                x: min_x,
                y: min_y,
                w: ((max_x - min_x).max(2)) as u32,
                h: ((max_y - min_y).max(2)) as u32,
            }
        }
        _ => Area {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        },
    }
}

fn is_full_virtual(a: &Area) -> bool {
    let v = full_virtual_area();
    a.x == v.x && a.y == v.y && a.w == v.w && a.h == v.h
}

/// Resolve the quick default source (hotkey / tray) into an Area.
pub fn default_source_area(s: &AppSettings) -> Area {
    let monitors = crate::screenshot::list_monitors().unwrap_or_default();
    let norm = |mut a: Area| {
        a.w = even(a.w.max(64));
        a.h = even(a.h.max(64));
        a
    };
    match s.rec_default_source.as_str() {
        "all" => norm(full_virtual_area()),
        m if m.starts_with("monitor:") => {
            let idx: usize = m["monitor:".len()..].parse().unwrap_or(s.rec_monitor);
            match monitors.get(idx).or_else(|| monitors.first()) {
                Some(mo) => norm(Area {
                    x: mo.x,
                    y: mo.y,
                    w: mo.width,
                    h: mo.height,
                }),
                None => norm(full_virtual_area()),
            }
        }
        "last_area" => match &s.rec_last_area {
            Some(a) if a.w >= 64 && a.h >= 64 => norm(Area {
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
            }),
            _ => norm(full_virtual_area()),
        },
        "window" => match crate::screenshot::active_window_info() {
            Ok(w) if w.width >= 64 && w.height >= 64 => norm(Area {
                x: w.x,
                y: w.y,
                w: w.width,
                h: w.height,
            }),
            _ => norm(full_virtual_area()),
        },
        _ => {
            // fullscreen = primary monitor
            match monitors.first() {
                Some(mo) => norm(Area {
                    x: mo.x,
                    y: mo.y,
                    w: mo.width,
                    h: mo.height,
                }),
                None => norm(full_virtual_area()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "windows")]
    fn replay_ring_rotate_and_save_window() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        // Exercises the exact primitives replay save uses (segment sets +
        // keep-last merge) without needing a Tauri AppHandle.
        let dir = std::env::temp_dir().join(format!("openscreen-ringtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = crate::settings::load_settings();
        let probe = get_probe(false).expect("native probe failed");
        let area = full_virtual_area();
        let params = resolve_params(&s, &probe, area, true).expect("params failed");
        let targets = resolve_targets(&params.area, params.out_w, params.out_h).expect("targets failed");
        assert_eq!(targets.len(), 1, "expected single monitor here");
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(10));
        let mut last_message = String::new();
        // Two rotations â‰ˆ two closed sets; the SAME pump is retargeted
        // (gapless audio across the joint, like the real supervisor).
        let (segs, pump) =
            Recorder::start_seg_set(&params, &targets, &dir, 1, None, &mut last_message).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let f1 = finish_segs(segs);
        let (segs, pump) =
            Recorder::start_seg_set(&params, &targets, &dir, 2, pump, &mut last_message).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let f2 = finish_segs(segs);
        if let Some(p) = pump {
            p.stop_and_join();
        }
        let _ = wiggler.join();
        let sets = vec![
            SegSet {
                files: f1.into_iter().filter(|f| valid_part(&f.path)).map(|f| (f.monitor_idx, f.path)).collect(),
                finished_at: std::time::SystemTime::now(),
            },
            SegSet {
                files: f2.into_iter().filter(|f| valid_part(&f.path)).map(|f| (f.monitor_idx, f.path)).collect(),
                finished_at: std::time::SystemTime::now(),
            },
        ];
        assert!(sets.iter().all(|s| !s.files.is_empty()), "empty segment set");
        let mut per_mon: std::collections::BTreeMap<usize, Vec<PathBuf>> = Default::default();
        for set in &sets {
            for (mi, p) in &set.files {
                per_mon.entry(*mi).or_default().push(p.clone());
            }
        }
        let out = dir.join("replay.mp4");
        let info = crate::mp4mux::remux_segments_keep_last(&per_mon[&0], &out, 3.0).expect("ring merge failed");
        eprintln!("ring save: {info:?}");
        assert!(info.duration_sec >= 2.0 && info.duration_sec <= 4.5, "bad window: {}", info.duration_sec);
        let back = crate::mp4info::read_info(&out).expect("ring file unreadable");
        assert!(back.video.is_some());
        // Cleanup temp.
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn apply_post_action(app: &tauri::AppHandle, action: &str, file: &Path) {
    let path = file.to_string_lossy().to_string();
    match action {
        "open_file" => {
            let _ = crate::procutil::cmd("cmd")
                .args(["/C", "start", "", &path])
                .spawn();
        }
        "open_folder" => {
            let _ = crate::procutil::cmd("explorer")
                .args(["/select,", &path])
                .spawn();
        }
        "copy_path" => {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(path);
            }
        }
        _ => {}
    }
    let _ = app;
}

/// Remove stale temp recording data at startup.
pub fn cleanup_temp() {
    let tmp = std::env::temp_dir();
    let _ = std::fs::remove_dir_all(tmp.join("openscreen-replay"));
    if let Ok(rd) = std::fs::read_dir(&tmp) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("openscreen-rec-") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}
