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
    Arc, Mutex as StdMutex,
};
use std::time::{Duration, Instant};

use crate::settings::{self, AppSettings};
use tauri::Emitter;

// ---------------------------------------------------------------------------
// ffmpeg discovery / download
// ---------------------------------------------------------------------------

const FFMPEG_URL: &str =
    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

fn ffmpeg_exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn bundled_ffmpeg() -> PathBuf {
    settings::app_dir().join("ffmpeg").join(ffmpeg_exe_name())
}

fn path_search(name: &str) -> Option<PathBuf> {
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn ffmpeg_path() -> Option<PathBuf> {
    // 1) Shipped inside the installer — always preferred, no download needed.
    if let Some(p) = crate::settings::bundled_file("ffmpeg/ffmpeg.exe") {
        return Some(p);
    }
    if let Some(p) = path_search(ffmpeg_exe_name()) {
        return Some(p);
    }
    let b = bundled_ffmpeg();
    if b.exists() {
        return Some(b);
    }
    None
}

fn ffmpeg_version(ff: &Path) -> String {
    crate::procutil::cmd(ff)
        .arg("-version")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
                .into()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Probe (encoders + audio devices), cached
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub ffmpeg: String,
    pub version: String,
    pub h264: Vec<String>,
    pub hevc: Vec<String>,
    pub av1: Vec<String>,
    pub audio_devices: Vec<String>,
    pub system_hint: Option<String>,
}

fn probe_path() -> PathBuf {
    settings::app_dir().join("ffmpeg_probe.json")
}

fn run_probe(ff: &Path) -> Probe {
    let mut p = Probe {
        ffmpeg: ff.to_string_lossy().to_string(),
        version: ffmpeg_version(ff),
        ..Default::default()
    };
    if let Ok(o) = crate::procutil::cmd(ff)
        .args(["-hide_banner", "-encoders"])
        .output()
    {
        let txt = String::from_utf8_lossy(&o.stdout);
        for line in txt.lines() {
            for enc in [
                "h264_nvenc",
                "h264_qsv",
                "h264_amf",
                "libx264",
                "hevc_nvenc",
                "hevc_qsv",
                "hevc_amf",
                "libx265",
                "av1_nvenc",
                "av1_qsv",
                "av1_amf",
            ] {
                if line.contains(enc) {
                    let list = if enc.starts_with("h264") {
                        &mut p.h264
                    } else if enc.starts_with("hevc") || enc == "libx265" {
                        &mut p.hevc
                    } else {
                        &mut p.av1
                    };
                    if !list.iter().any(|e| e == enc) {
                        list.push(enc.to_string());
                    }
                }
            }
        }
    }
    // DirectShow audio devices (Windows). Best-effort: video-only if none.
    #[cfg(target_os = "windows")]
    {
        if let Ok(o) = crate::procutil::cmd(ff)
            .args(["-list_devices", "true", "-f", "dshow", "-i", "dummy"])
            .output()
        {
            let txt = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let mut in_audio = false;
            for line in txt.lines() {
                let l = line.trim().to_lowercase();
                if l.contains("directshow audio devices") {
                    in_audio = true;
                    continue;
                }
                if l.contains("directshow video devices") {
                    in_audio = false;
                    continue;
                }
                if in_audio {
                    // lines look like: [dshow ...]  "Microphone (XYZ)"
                    if let Some(a) = line.find('"') {
                        if let Some(b) = line[a + 1..].find('"') {
                            let name = line[a + 1..a + 1 + b].to_string();
                            if !name.is_empty() && name != "dummy" {
                                p.audio_devices.push(name);
                            }
                        }
                    }
                }
            }
        }
        // Heuristic system-audio (loopback) endpoint.
        p.system_hint = p
            .audio_devices
            .iter()
            .find(|n| {
                let l = n.to_lowercase();
                l.contains("stereo mix")
                    || l.contains("what u hear")
                    || l.contains("loopback")
                    || l.contains("mix")
                    || l.contains("wave out")
            })
            .cloned();
    }
    p
}

pub fn get_probe(refresh: bool) -> Result<Probe, String> {
    let ff = ffmpeg_path().ok_or_else(|| {
        "Recorder engine (ffmpeg) is missing. Open Settings → Recording and download it once (~80MB). / محرك التسجيل غير موجود — حمّله من الإعدادات.".to_string()
    })?;
    if !refresh {
        if let Ok(bytes) = std::fs::read(probe_path()) {
            if let Ok(mut p) = serde_json::from_slice::<Probe>(&bytes) {
                if p.ffmpeg == ff.to_string_lossy().to_string() {
                    // refresh device list cheaply? keep cache for speed
                    return Ok(std::mem::take(&mut p));
                }
            }
        }
    }
    let p = run_probe(&ff);
    let _ = std::fs::create_dir_all(settings::app_dir());
    if let Ok(bytes) = serde_json::to_vec_pretty(&p) {
        let _ = std::fs::write(probe_path(), bytes);
    }
    Ok(p)
}

fn hw_label(e: &str) -> &'static str {
    if e.contains("nvenc") {
        "NVIDIA NVENC"
    } else if e.contains("qsv") {
        "Intel Quick Sync"
    } else if e.contains("amf") {
        "AMD AMF"
    } else {
        "Software (CPU)"
    }
}

fn is_hw_encoder(e: &str) -> bool {
    e.contains("nvenc") || e.contains("qsv") || e.contains("amf")
}

#[derive(Debug, Clone)]
struct EncCand {
    name: String,
    label: String,
}

/// Encoder fallback chain: hardware first (as chosen/probed), software last.
/// A listed encoder is NOT proof it can open (e.g. NVENC without NVIDIA GPU),
/// so every candidate is actually tried at startup — first success wins.
fn encoder_chain(probe: &Probe, codec: &str) -> Vec<EncCand> {
    let mut chain: Vec<EncCand> = vec![];
    let mut push = |names: &[&str], avail: &[String]| {
        for n in names {
            if avail.iter().any(|e| e == n) && !chain.iter().any(|c: &EncCand| c.name == *n) {
                chain.push(EncCand {
                    name: n.to_string(),
                    label: hw_label(n).to_string(),
                });
            }
        }
    };
    match codec {
        "hevc" => {
            push(&["hevc_nvenc", "hevc_qsv", "hevc_amf", "libx265"], &probe.hevc);
        }
        "av1" => {
            // Hardware AV1 only — software AV1 is too heavy for realtime.
            push(&["av1_nvenc", "av1_qsv", "av1_amf"], &probe.av1);
        }
        _ => {}
    }
    // H.264 is always the final fallback (most compatible).
    push(
        &["h264_nvenc", "h264_qsv", "h264_amf", "libx264"],
        &probe.h264,
    );
    if chain.is_empty() {
        chain.push(EncCand {
            name: "libx264".to_string(),
            label: "Software (CPU)".to_string(),
        });
    }
    chain
}

// ---------------------------------------------------------------------------
// One-time ffmpeg download (~80MB essentials build, ffmpeg.exe only)
// ---------------------------------------------------------------------------

pub async fn ensure_ffmpeg(app: tauri::AppHandle) -> Result<String, String> {
    if let Some(p) = ffmpeg_path() {
        return Ok(p.to_string_lossy().to_string());
    }
    let dir = settings::app_dir().join("ffmpeg");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .user_agent("OpenScreen/0.1")
        .build()
        .map_err(|e| e.to_string())?;
    // Mirror list: official build first, GitHub mirrors after (some networks
    // block individual hosts).
    let mut urls = vec![FFMPEG_URL.to_string()];
    urls.extend(github_ffmpeg_mirrors(&client).await);
    let mut last_err = String::from("download failed");
    for url in urls {
        match try_fetch_ffmpeg(&client, &url, &dir, &app).await {
            Ok(p) => return Ok(p.to_string_lossy().to_string()),
            Err(e) => {
                last_err = e;
            }
        }
    }
    Err(format!("{last_err}. Check internet and retry. / فشل التحميل — تحقق من الإنترنت."))
}

/// Discover ffmpeg Windows builds on GitHub mirrors (Gyan releases).
async fn github_ffmpeg_mirrors(client: &reqwest::Client) -> Vec<String> {
    let mut out = vec![];
    let api = "https://api.github.com/repos/GyanD/codeffmpeg/releases/latest";
    if let Ok(resp) = client
        .get(api)
        .header("User-Agent", "OpenScreen/0.1")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        if let Ok(bytes) = resp.bytes().await {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(assets) = v.get("assets").and_then(|a| a.as_array()) {
                    for a in assets {
                        if let Some(url) =
                            a.get("browser_download_url").and_then(|u| u.as_str())
                        {
                            if url.contains("essentials_build.zip") {
                                out.push(url.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

async fn try_fetch_ffmpeg(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
    app: &tauri::AppHandle,
) -> Result<PathBuf, String> {
    let zip_path = dir.join("ffmpeg-release-essentials.zip");
    let resp = client.get(url).send().await.map_err(|e| {
        format!("error sending request for url ({url}). {e}")
    })?;
    if !resp.status().is_success() {
        return Err(format!("Download failed (HTTP {}).", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(&zip_path)
        .await
        .map_err(|e| e.to_string())?;
    use tokio::io::AsyncWriteExt;
    let mut done: u64 = 0;
    let mut last_pct: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        file.write_all(&bytes).await.map_err(|e| e.to_string())?;
        done += bytes.len() as u64;
        if total > 0 {
            let pct = done * 100 / total;
            if pct >= last_pct + 2 {
                last_pct = pct;
                let _ = app.emit(
                    "openscreen:ffmpeg-progress",
                    serde_json::json!({ "pct": pct, "done": true }),
                );
            }
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    // Extract ffmpeg.exe only.
    let dir_c = dir.to_path_buf();
    let zip_c = zip_path.clone();
    let found = tokio::task::spawn_blocking(move || {
        let f = std::fs::File::open(&zip_c).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let name = entry.name().replace('\\', "/");
            if name.to_lowercase().ends_with("/ffmpeg.exe") || name.to_lowercase() == "ffmpeg.exe" {
                let out = dir_c.join("ffmpeg.exe");
                let mut w = std::fs::File::create(&out).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut w).map_err(|e| e.to_string())?;
                return Ok::<PathBuf, String>(out);
            }
        }
        Err("ffmpeg.exe not found in archive".to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = std::fs::remove_file(dir.join("ffmpeg-release-essentials.zip"));
    // Verify it runs.
    let v = ffmpeg_version(&found);
    if v.is_empty() {
        return Err("Downloaded ffmpeg does not run on this PC.".to_string());
    }
    let _ = app.emit(
        "openscreen:ffmpeg-progress",
        serde_json::json!({ "pct": 100, "done": true }),
    );
    // Prime the probe cache now.
    let probe = run_probe(&found);
    if let Ok(bytes) = serde_json::to_vec_pretty(&probe) {
        let _ = std::fs::write(probe_path(), bytes);
    }
    Ok(found)
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
    pub desktop: bool, // capture the whole virtual desktop (no offsets)
    pub codec: String, // normalized choice: auto|h264|hevc|av1
    pub out_w: u32,
    pub out_h: u32,
    pub fps: u32,
    pub encoder: String,
    pub encoder_label: String,
    pub sw_fallback_note: bool,
    pub bitrate_k: u32,
    pub audio: String,
    pub mic: Option<String>,
    pub sys: Option<String>,
    pub sample_rate: u32,
    pub channels: u32,
    pub cursor: bool,
    pub power_note: Option<String>,
}

fn even(n: u32) -> u32 {
    (n.max(2) / 2) * 2
}

fn bitrate_for(quality: &str, height: u32) -> u32 {
    // kbps ladder
    match quality {
        "low" => {
            if height >= 1080 {
                5000
            } else if height >= 720 {
                3000
            } else {
                1500
            }
        }
        "high" => {
            if height >= 1080 {
                12000
            } else if height >= 720 {
                10000
            } else {
                5000
            }
        }
        "veryhigh" => {
            if height >= 1080 {
                20000
            } else if height >= 720 {
                16000
            } else {
                8000
            }
        }
        _ => {
            // balanced
            if height >= 1080 {
                8000
            } else if height >= 720 {
                6000
            } else {
                3000
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
    let pick_mic = || -> Option<String> {
        if !s.rec_mic_device.trim().is_empty()
            && probe.audio_devices.iter().any(|d| d == &s.rec_mic_device)
        {
            return Some(s.rec_mic_device.clone());
        }
        // First non-loopback device as default mic.
        probe
            .audio_devices
            .iter()
            .find(|d| {
                let l = d.to_lowercase();
                !(l.contains("stereo mix")
                    || l.contains("what u hear")
                    || l.contains("loopback")
                    || l.contains("wave out"))
            })
            .cloned()
            .or_else(|| probe.audio_devices.first().cloned())
    };
    match mode.as_str() {
        "mic" | "microphone" => ("mic".to_string(), pick_mic(), None),
        "system" => ("system".to_string(), None, probe.system_hint.clone()),
        "both" => (
            "both".to_string(),
            pick_mic(),
            probe.system_hint.clone(),
        ),
        _ => ("none".to_string(), None, None),
    }
}

fn resolve_params(
    s: &AppSettings,
    probe: &Probe,
    area: Area,
    replay: bool,
) -> Params {
    // FPS (never 120 — only reliably supported options).
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
    // Resolution.
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
    if out_h > area.h && res_key != "custom" {
        out_h = area.h; // never upscale for presets
    }
    let mut out_w = if res_key == "custom" && !replay {
        cw.max(320).min(3840)
    } else if res_key == "source" {
        area.w
    } else {
        ((area.w as u64 * out_h as u64) / area.h.max(1) as u64) as u32
    };
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
    // Codec -> fallback chain; start with the first candidate.
    let codec = if replay {
        "h264".to_string()
    } else {
        match s.rec_codec.as_str() {
            "hevc" | "av1" => s.rec_codec.clone(),
            _ => "auto".to_string(),
        }
    };
    let chain = encoder_chain(probe, &codec);
    let first = &chain[0];
    let (encoder, encoder_label) = (first.name.clone(), first.label.clone());
    let sw_fallback_note = !is_hw_encoder(&encoder);    // Audio.
    let (audio, mic, sys) = resolve_audio(s, probe, replay);
    // Power saving overrides.
    let mut power_note = None;
    let mut fps_out = fps;
    let mut bitrate_out = bitrate_k;
    if !replay && s.rec_power_saving {
        fps_out = fps.min(30);
        bitrate_out = bitrate_k.min(bitrate_for("low", out_h));
        power_note = Some("Power saving active: 30 FPS, lower bitrate.".to_string());
    }
    Params {
        area: Area {
            x: area.x,
            y: area.y,
            w: even(area.w),
            h: even(area.h),
        },
        desktop: false, // set by the caller (full virtual desktop => no offsets)
        codec,
        out_w,
        out_h,
        fps: fps_out,
        encoder,
        encoder_label,
        sw_fallback_note,
        bitrate_k: bitrate_out,
        audio,
        mic,
        sys,
        sample_rate: if s.rec_sample_rate == 44100 { 44100 } else { 48000 },
        channels: if s.rec_channels == 1 { 1 } else { 2 },
        cursor: s.rec_cursor,
        power_note,
    }
}

// ---------------------------------------------------------------------------
// Live stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct LiveStats {
    frames: u64,
    frames_base: u64,
    fps_now: f32,
    size_bytes: u64,
    sys_db: Option<f32>,
    mic_db: Option<f32>,
    err_tail: String, // last stderr lines — real ffmpeg diagnostics
}

// ---------------------------------------------------------------------------
// Recorder state
// ---------------------------------------------------------------------------

struct ActiveRec {
    child: Option<tokio::process::Child>,
    partials: Vec<PathBuf>,
    workdir: PathBuf,
    active_ms: u64,
    active_start: Option<Instant>,
    params: Params,
    live: Arc<StdMutex<LiveStats>>,
    final_name: String,
}

struct ActiveReplay {
    child: Option<tokio::process::Child>,
    segdir: PathBuf,
    started_at: Instant,
    duration_s: u64,
    params: Params,
    live: Arc<StdMutex<LiveStats>>,
    stop_flag: Arc<AtomicBool>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

pub struct Recorder {
    rec: Option<ActiveRec>,
    replay: Option<ActiveReplay>,
    rec_icon: Option<(Vec<u8>, u32, u32)>,
    timer_gen: Arc<AtomicU64>,
    timer_handle: Option<tokio::task::AbortHandle>,
    last_message: String,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            rec: None,
            replay: None,
            rec_icon: None,
            timer_gen: Arc::new(AtomicU64::new(0)),
            timer_handle: None,
            last_message: String::new(),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.rec.as_ref().map(|r| r.child.is_some()).unwrap_or(false)
    }

    pub fn is_paused(&self) -> bool {
        match &self.rec {
            Some(r) => r.child.is_none(),
            None => false,
        }
    }

    pub fn replay_state(&self) -> &'static str {
        match &self.replay {
            None => "off",
            Some(r) => {
                if r.child.is_none() {
                    "saving"
                } else if r.started_at.elapsed().as_secs() < 3 {
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
    pub width: u32,
    pub height: u32,
    pub encoder: String,
    pub encoder_label: String,
    pub size_bytes: u64,
    pub dropped: u64,
    pub sys_db: Option<f32>,
    pub mic_db: Option<f32>,
    pub message: String,
    pub ffmpeg: bool,
    pub ffmpeg_path: Option<String>,
    pub power_note: Option<String>,
}

pub(crate) fn snapshot(rec: &Recorder) -> RecStatus {
    status_of(rec)
}

fn status_of(rec: &Recorder) -> RecStatus {    let ff = ffmpeg_path();
    if let Some(r) = &rec.rec {
        let live = r.live.lock().map(|l| l.clone()).unwrap_or_default();
        let mut elapsed_ms = r.active_ms;
        if let Some(t) = r.active_start {
            elapsed_ms += t.elapsed().as_millis() as u64;
        }
        let elapsed_s = elapsed_ms / 1000;
        let frames = live.frames_base + live.frames;
        let dropped = (elapsed_s * r.params.fps as u64).saturating_sub(frames);
        return RecStatus {
            recording: r.child.is_some(),
            paused: r.child.is_none(),
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
            width: r.params.out_w,
            height: r.params.out_h,
            encoder: r.params.encoder.clone(),
            encoder_label: r.params.encoder_label.clone(),
            size_bytes: live.size_bytes,
            dropped,
            sys_db: live.sys_db,
            mic_db: live.mic_db,
            message: rec.last_message.clone(),
            ffmpeg: ff.is_some(),
            ffmpeg_path: ff.map(|p| p.to_string_lossy().to_string()),
            power_note: r.params.power_note.clone(),
        };
    }
    if let Some(rp) = &rec.replay {
        let live = rp.live.lock().map(|l| l.clone()).unwrap_or_default();
        return RecStatus {
            recording: false,
            paused: false,
            replay: rec.replay_state().to_string(),
            replay_ready_s: rp.started_at.elapsed().as_secs(),
            replay_duration: rp.duration_s,
            elapsed_s: 0,
            frames: live.frames_base + live.frames,
            fps: rp.params.fps,
            width: rp.params.out_w,
            height: rp.params.out_h,
            encoder: rp.params.encoder.clone(),
            encoder_label: rp.params.encoder_label.clone(),
            size_bytes: live.size_bytes,
            dropped: 0,
            sys_db: live.sys_db,
            mic_db: live.mic_db,
            message: rec.last_message.clone(),
            ffmpeg: ff.is_some(),
            ffmpeg_path: ff.map(|p| p.to_string_lossy().to_string()),
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
        width: 0,
        height: 0,
        encoder: String::new(),
        encoder_label: String::new(),
        size_bytes: 0,
        dropped: 0,
        sys_db: None,
        mic_db: None,
        message: rec.last_message.clone(),
        ffmpeg: ff.is_some(),
        ffmpeg_path: ff.map(|p| p.to_string_lossy().to_string()),
        power_note: None,
    }
}

// ---------------------------------------------------------------------------
// ffmpeg command building
// ---------------------------------------------------------------------------

struct BuiltCmd {
    args: Vec<String>,
    has_sys: bool,
    has_mic: bool,
}

fn build_cmd(ff: &Path, p: &Params, out: &Path, segment_time: Option<u64>) -> BuiltCmd {
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
    ];
    // Video input.
    a.extend([
        "-f".into(),
        "gdigrab".into(),
        "-framerate".into(),
        p.fps.to_string(),
        "-draw_mouse".into(),
        if p.cursor { "1".into() } else { "0".into() },
    ]);
    let all_desktop = p.desktop;
    if !all_desktop {
        a.extend([
            "-offset_x".into(),
            p.area.x.to_string(),
            "-offset_y".into(),
            p.area.y.to_string(),
            "-video_size".into(),
            format!("{}x{}", p.area.w, p.area.h),
        ]);
    }
    a.extend(["-i".into(), "desktop".into()]);
    // Audio inputs.
    let mut audio_inputs: Vec<(&str, String)> = vec![]; // (kind, device)
    if p.audio == "mic" || p.audio == "both" {
        if let Some(m) = &p.mic {
            audio_inputs.push(("mic", m.clone()));
        }
    }
    if p.audio == "system" || p.audio == "both" {
        if let Some(s) = &p.sys {
            audio_inputs.push(("sys", s.clone()));
        }
    }
    for (_, dev) in &audio_inputs {
        a.extend([
            "-rtbufsize".into(),
            "50M".into(),
            "-f".into(),
            "dshow".into(),
            "-i".into(),
            format!("audio=\"{dev}\""),
        ]);
    }
    let has_sys = audio_inputs.iter().any(|(k, _)| *k == "sys");
    let has_mic = audio_inputs.iter().any(|(k, _)| *k == "mic");
    // Filter graph.
    let need_scale = p.out_w != p.area.w || p.out_h != p.area.h;
    if audio_inputs.is_empty() {
        if need_scale {
            a.extend([
                "-vf".into(),
                format!("scale={}:{}", p.out_w, p.out_h),
            ]);
        }
        a.push("-an".into());
    } else {
        // [0:v] scale + per-source ebur128 meters + amix when both.
        let mut g = String::new();
        if need_scale {
            g.push_str(&format!("[0:v]scale={}:{}[vout];", p.out_w, p.out_h));
        }
        // audio input indexes: desktop=0, then each dshow input in order
        let mut labels: Vec<String> = vec![];
        for (i, (kind, _)) in audio_inputs.iter().enumerate() {
            let idx = i + 1;
            let tag = if *kind == "sys" { "sys" } else { "mic" };
            g.push_str(&format!("[{idx}:a]ebur128=peak=true[{tag}];"));
            labels.push(tag.to_string());
        }
        let amap = if labels.len() > 1 {
            g.push_str("[sys][mic]amix=inputs=2:duration=longest:dropout_transition=0[aout];");
            "[aout]".to_string()
        } else {
            format!("[{}]", labels[0])
        };
        // strip trailing ';'
        if g.ends_with(';') {
            g.pop();
        }
        a.extend(["-filter_complex".into(), g]);
        if need_scale {
            a.extend(["-map".into(), "[vout]".into()]);
        } else {
            a.extend(["-map".into(), "0:v".into()]);
        }
        a.extend(["-map".into(), amap]);
        a.extend([
            "-ar".into(),
            p.sample_rate.to_string(),
            "-ac".into(),
            p.channels.to_string(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "128k".into(),
        ]);
    }
    // Video encode.
    a.extend([
        "-c:v".into(),
        p.encoder.clone(),
        "-b:v".into(),
        format!("{}k", p.bitrate_k),
        "-pix_fmt".into(),
        "yuv420p".into(),
    ]);
    if p.encoder == "libx264" || p.encoder == "libx265" {
        a.extend(["-preset".into(), "veryfast".into()]);
    }
    a.extend(["-r".into(), p.fps.to_string()]);
    a.extend(["-progress".into(), "pipe:1".into(), "-nostats".into()]);
    // Output.
    if let Some(seg) = segment_time {
        a.extend([
            "-f".into(),
            "segment".into(),
            "-segment_time".into(),
            seg.to_string(),
            "-segment_format".into(),
            "mp4".into(),
            out.to_string_lossy().to_string(),
        ]);
    } else {
        a.extend([
            "-movflags".into(),
            "+faststart".into(),
            out.to_string_lossy().to_string(),
        ]);
    }
    let _ = ff;
    BuiltCmd { args: a, has_sys, has_mic }
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

async fn spawn_ffmpeg(ff: &Path, args: Vec<String>) -> Result<tokio::process::Child, String> {
    crate::procutil::tokio_cmd(ff)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Unable to start screen recording. ({e})"))
}

fn spawn_readers(
    child: &mut tokio::process::Child,
    live: Arc<StdMutex<LiveStats>>,
    has_sys: bool,
    has_mic: bool,
) -> Vec<tokio::task::JoinHandle<()>> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut handles = vec![];
    if let Some(out) = child.stdout.take() {
        let live = live.clone();
        handles.push(tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            let mut key = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() || line == "." {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    key = k.trim().to_string();
                    let v = v.trim();
                    if let Ok(mut l) = live.lock() {
                        match key.as_str() {
                            "frame" => {
                                l.frames = v.parse().unwrap_or(l.frames);
                            }
                            "fps" => {
                                l.fps_now = v.parse().unwrap_or(l.fps_now);
                            }
                            "total_size" => {
                                l.size_bytes = v.parse().unwrap_or(l.size_bytes);
                            }
                            _ => {}
                        }
                    }
                } else {
                    let _ = &key;
                }
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let live = live.clone();
        handles.push(tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Keep a capped tail of stderr: real diagnostics on failure.
                if let Ok(mut l) = live.lock() {
                    l.err_tail.push_str(&line);
                    l.err_tail.push('\n');
                    const CAP: usize = 3000;
                    if l.err_tail.len() > CAP {
                        l.err_tail = l.err_tail[l.err_tail.len() - CAP..].to_string();
                    }
                }
                // ebur128 loudness: [Parsed_ebur128_0 @ ...] M: -23.4 S: ...
                if line.contains("ebur128_") {
                    let idx = if line.contains("ebur128_1") { 1 } else { 0 };
                    if let Some(pos) = line.find("M:") {
                        let num: String = line[pos + 2..]
                            .chars()
                            .take_while(|c| c.is_numeric() || *c == '.' || *c == '-')
                            .collect();
                        if let Ok(db) = num.parse::<f32>() {
                            if let Ok(mut l) = live.lock() {
                                // filter order: sys first when both present
                                let is_sys = if has_sys && has_mic {
                                    idx == 0
                                } else {
                                    has_sys
                                };
                                if is_sys {
                                    l.sys_db = Some(db);
                                } else {
                                    l.mic_db = Some(db);
                                }
                            }
                        }
                    }
                }
            }
        }));
    }
    handles
}

/// ffmpeg's own words about a failure (last ~400 chars of stderr).
fn short_tail(live: &Arc<StdMutex<LiveStats>>) -> String {
    let tail = live
        .lock()
        .map(|l| l.err_tail.clone())
        .unwrap_or_default();
    let t = tail.trim();
    if t.is_empty() {
        return String::new();
    }
    t.chars()
        .rev()
        .take(400)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Did the child survive startup? Polls so dead-on-arrival processes
/// (bad args / dead audio / unusable encoder) fail fast instead of waiting.
async fn verify_alive(child: &mut tokio::process::Child, ms: u64) -> bool {
    let mut waited = 0;
    while waited < ms {
        tokio::time::sleep(Duration::from_millis(200)).await;
        waited += 200;
        match child.try_wait() {
            Ok(None) => continue,
            _ => return false,
        }
    }
    matches!(child.try_wait(), Ok(None))
}

struct Spawned {
    child: tokio::process::Child,
    live: Arc<StdMutex<LiveStats>>,
}

/// Spawn ffmpeg and VERIFY it actually runs, walking the encoder fallback
/// chain (HW in probe order, software last). A listed-but-unusable encoder
/// (e.g. NVENC without NVIDIA GPU) fails here and the next one is tried.
/// Audio failures retry video-only. First success wins.
async fn spawn_verified(
    ff: &Path,
    probe: &Probe,
    params: &mut Params,
    out: &Path,
    segment: Option<u64>,
    reuse_live: Option<Arc<StdMutex<LiveStats>>>,
) -> Result<Spawned, String> {
    let chain = encoder_chain(probe, &params.codec);
    let orig_audio = params.audio.clone();
    let orig_mic = params.mic.clone();
    let orig_sys = params.sys.clone();
    let mut last_err = String::new();
    for cand in &chain {
        params.encoder = cand.name.clone();
        params.encoder_label = cand.label.clone();
        for attempt in 0..2 {
            if attempt == 0 || orig_audio == "none" {
                params.audio = orig_audio.clone();
                params.mic = orig_mic.clone();
                params.sys = orig_sys.clone();
                if attempt > 0 {
                    break; // no second attempt when audio was never requested
                }
            } else {
                params.audio = "none".to_string();
                params.mic = None;
                params.sys = None;
            }
            let cmd = build_cmd(ff, params, out, segment);
            let mut child = spawn_ffmpeg(ff, cmd.args.clone()).await?;
            let live = reuse_live
                .clone()
                .unwrap_or_else(|| Arc::new(StdMutex::new(LiveStats::default())));
            let _readers =
                spawn_readers(&mut child, live.clone(), cmd.has_sys, cmd.has_mic);
            if verify_alive(&mut child, if segment.is_some() { 2000 } else { 1500 }).await {
                return Ok(Spawned { child, live });
            }
            let tail = short_tail(&live);
            let _ = child.start_kill();
            let _ = child.wait().await;
            last_err = tail.clone();
            // Route the failure: broken VIDEO encoder -> next encoder now
            // (audio retry would just waste time); otherwise retry video-only.
            let tl = tail.to_lowercase();
            let video_broken = tl.contains("could not open encoder")
                || tl.contains("error initializing output stream 0:0")
                || tl.contains("no nvenc")
                || tl.contains("nvenc");
            if video_broken || orig_audio == "none" {
                break;
            }
        }
    }
    let detail = if last_err.is_empty() {
        String::new()
    } else {
        format!(" ({last_err})")
    };
    Err(format!("Unable to start screen recording.{detail}"))
}

/// Ask ffmpeg to finish cleanly ('q'), else kill.
async fn quit_child(child: &mut tokio::process::Child) {
    use tokio::io::AsyncWriteExt;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(b"q").await;
        let _ = stdin.flush().await;
    }
    match tokio::time::timeout(Duration::from_secs(8), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

async fn concat_copy(ff: &Path, files: &[PathBuf], out: &Path) -> Result<(), String> {
    if files.is_empty() {
        return Err("Nothing was recorded.".to_string());
    }
    if files.len() == 1 {
        if std::fs::rename(&files[0], out).is_err() {
            std::fs::copy(&files[0], out).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&files[0]);
        }
        return Ok(());
    }
    let list_path = out.with_extension("filelist.txt");
    let mut list = String::new();
    for f in files {
        list.push_str(&format!("file '{}'\n", f.to_string_lossy().replace('\'', "'\\''")));
    }
    std::fs::write(&list_path, list).map_err(|e| e.to_string())?;
    let st = crate::procutil::tokio_cmd(ff)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            &list_path.to_string_lossy(),
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            &out.to_string_lossy(),
        ])
        .status()
        .await
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&list_path);
    if !st.success() {
        return Err("Could not finalize the recording file.".to_string());
    }
    for f in files {
        let _ = std::fs::remove_file(f);
    }
    Ok(())
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
            return Err("Not enough disk space to save this recording. Free up space and try again. / لا توجد مساحة كافية لحفظ التسجيل.".to_string());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recording history (metadata only — videos never loaded to RAM)
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
                    let rec = shared_c.lock().await;
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
        let ff = ffmpeg_path().ok_or_else(|| {
            "Recorder engine (ffmpeg) is missing. Open Settings → Recording and download it once. / محرك التسجيل غير موجود.".to_string()
        })?;
        let folder = default_folder(&s);
        std::fs::create_dir_all(&folder).map_err(|_| {
            "Unable to create the recording folder. Choose another folder in Settings. / تعذّر إنشاء مجلد التسجيل.".to_string()
        })?;
        check_disk(&folder, 1024 * 1024 * 1024)?; // 1 GB for open-ended recording
        let probe = get_probe(false)?;
        let mut params = resolve_params(&s, &probe, area.clone(), false);
        params.desktop = is_full_virtual(&params.area);
        if params.audio != "none" && params.mic.is_none() && params.sys.is_none() {
            // Audio requested but no device: keep video, say so (don't fail video).
            params.audio = "none".to_string();
            self.note("Audio capture is unavailable — recording video only.");
        }
        let workdir = temp_workdir("openscreen-rec");
        let part = workdir.join("part001.mp4");
        let wanted_audio = params.audio.clone();
        let cands = encoder_chain(&probe, &params.codec);
        let first_name = cands[0].name.clone();
        let first_label = cands[0].label.clone();
        let spawned = spawn_verified(&ff, &probe, &mut params, &part, None, None)
            .await
            .map_err(|e| {
                self.note(&e);
                e
            })?;
        if params.encoder != first_name {
            if params.encoder == "libx264" || params.encoder == "libx265" {
                self.note("Hardware encoder unavailable. Using software encoding.");
            } else {
                self.note(&format!(
                    "{first_label} unavailable. Using {}.",
                    params.encoder_label
                ));
            }
        } else if wanted_audio != "none" && params.audio == "none" {
            self.note("Audio capture failed — recording video only.");
        }
        let Spawned { child, live } = spawned;
        self.rec = Some(ActiveRec {
            child: Some(child),
            partials: vec![],
            workdir,
            active_ms: 0,
            active_start: Some(Instant::now()),
            params: params.clone(),
            live,
            final_name: final_name(false),
        });
        self.start_timer(&shared, app);
        self.apply_tray(app);
        self.notify(
            app,
            "Open Screen — Recording",
            "Recording started. Use the tray menu or Ctrl+Shift+R to stop.",
        );
        if params.sw_fallback_note {
            self.note("Hardware encoder unavailable. Using software encoding.");
        }
        Ok(status_of(self))
    }

    pub async fn pause_recording(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let r = self.rec.as_mut().ok_or("Not recording.".to_string())?;
        if r.child.is_none() {
            return Err("Already paused.".to_string());
        }
        if let Some(mut child) = r.child.take() {
            quit_child(&mut child).await;
        }
        // Seal current partial.
        let idx = r.partials.len() + 1;
        let sealed = r.workdir.join(format!("part{idx:03}.mp4"));
        // The live partial was written to part001 path on first run; move it.
        let live_path = r.workdir.join("part001.mp4");
        if r.partials.is_empty() && live_path.exists() {
            let _ = std::fs::rename(&live_path, &sealed);
            r.partials.push(sealed);
        }
        if let Some(t) = r.active_start.take() {
            r.active_ms += t.elapsed().as_millis() as u64;
        }
        if let Ok(mut l) = r.live.lock() {
            l.frames_base += l.frames;
            l.frames = 0;
        }
        self.apply_tray(app);
        Ok(status_of(self))
    }

    pub async fn resume_recording(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let has = self.rec.as_ref().map(|r| r.child.is_some()).unwrap_or(false);
        if !self.rec.is_some() {
            return Err("Not recording.".to_string());
        }
        if has {
            return Err("Already recording.".to_string());
        }
        let ff = ffmpeg_path().ok_or("Recorder engine missing.".to_string())?;
        let (params, workdir, live, idx) = match &self.rec {
            Some(r) => (
                r.params.clone(),
                r.workdir.clone(),
                r.live.clone(),
                r.partials.len() + 1,
            ),
            None => return Err("Not recording.".to_string()),
        };
        // Next partial goes to a fresh file (never overwrite sealed ones).
        let part = workdir.join(format!("live{idx:03}.mp4"));
        let mut params = params;
        let probe = get_probe(false)?;
        let spawned = spawn_verified(&ff, &probe, &mut params, &part, None, Some(live.clone()))
            .await
            .map_err(|e| {
                self.note(&e);
                e
            })?;
        let Spawned { child, .. } = spawned;
        if let Some(r) = self.rec.as_mut() {
            // Rename live file into sealed slot on next pause/stop.
            r.child = Some(child);
            r.active_start = Some(Instant::now());
            // Track the live path via partials placeholder.
            r.partials.push(part);
        }
        self.apply_tray(app);
        Ok(status_of(self))
    }

    /// Stop recording, finalize file, history, post-action.
    pub async fn stop_recording(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let mut r = self.rec.take().ok_or("Not recording.".to_string())?;
        if let Some(mut child) = r.child.take() {
            quit_child(&mut child).await;
        }
        // Seal the live partial.
        let live_candidates = [
            r.workdir.join("part001.mp4"),
            r.workdir.join(format!("live{:03}.mp4", r.partials.len())),
        ];
        for c in live_candidates {
            if c.exists() && !r.partials.iter().any(|p| p == &c) {
                r.partials.push(c);
                break;
            }
        }
        r.partials.retain(|p| p.exists());
        // Drop empty/corrupt partials (e.g. paused instantly) so concat never chokes.
        r.partials.retain(|p| {
            let ok = std::fs::metadata(p).map(|m| m.len() > 4096).unwrap_or(false);
            if !ok {
                let _ = std::fs::remove_file(p);
            }
            ok
        });
        if r.partials.is_empty() {
            let _ = std::fs::remove_dir_all(&r.workdir);
            self.stop_timer();
            let tail = short_tail(&r.live);
            let msg = if tail.is_empty() {
                "Nothing was recorded.".to_string()
            } else {
                format!("Nothing was recorded. {tail}")
            };
            self.note(&msg);
            self.apply_tray(app);
            return Ok(status_of(self));
        }
        let s = settings::load_settings();
        let folder = default_folder(&s);
        let _ = std::fs::create_dir_all(&folder);
        let out = folder.join(&r.final_name);
        // Estimate need: current temp size + margin.
        let temp_size: u64 = r
            .partials
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        check_disk(&folder, temp_size + 100 * 1024 * 1024)?;
        let ff = ffmpeg_path().ok_or("Recorder engine missing.".to_string())?;
        concat_copy(&ff, &r.partials, &out)
            .await
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&r.workdir);
                e
            })?;
        let _ = std::fs::remove_dir_all(&r.workdir);
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let mut elapsed_ms = r.active_ms;
        if let Some(t) = r.active_start {
            elapsed_ms += t.elapsed().as_millis() as u64;
        }
        let item = RecHistoryItem {
            id: uuid::Uuid::new_v4().to_string(),
            name: r.final_name.clone(),
            path: out.to_string_lossy().to_string(),
            created_at: chrono::Local::now().to_rfc3339(),
            duration_s: elapsed_ms / 1000,
            width: r.params.out_w,
            height: r.params.out_h,
            fps: r.params.fps,
            size,
            kind: "screen".to_string(),
        };
        rec_history_add(item, s.rec_history_limit);
        self.stop_timer();
        self.apply_tray(app);
        self.notify(app, "Open Screen — Recording saved", &r.final_name);
        apply_post_action(app, &s.rec_post_action, &out);
        self.note("");
        Ok(status_of(self))
    }

    // -------------------------------------------------- Instant Replay ----
    pub async fn replay_start(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        if self.replay.is_some() {
            return Err("Instant Replay is already on.".to_string());
        }
        let s = settings::load_settings();
        let ff = ffmpeg_path().ok_or_else(|| {
            "Recorder engine (ffmpeg) is missing. Open Settings → Recording and download it once. / محرك التسجيل غير موجود.".to_string()
        })?;
        let probe = get_probe(false)?;
        // Replay captures the full virtual screen.
        let area = full_virtual_area();
        let mut params = resolve_params(&s, &probe, area, true);
        params.desktop = true; // replay always covers the full virtual screen
        if params.audio != "none" && params.mic.is_none() && params.sys.is_none() {
            params.audio = "none".to_string();
            self.note("Audio capture is unavailable — replay video only.");
        }
        let segdir = std::env::temp_dir().join("openscreen-replay");
        let _ = std::fs::remove_dir_all(&segdir);
        std::fs::create_dir_all(&segdir).map_err(|e| e.to_string())?;
        check_disk(&segdir, 512 * 1024 * 1024)?;
        let pattern = segdir.join("seg%05d.mp4");
        let wanted_audio = params.audio.clone();
        let cands = encoder_chain(&probe, &params.codec);
        let first_name = cands[0].name.clone();
        let first_label = cands[0].label.clone();
        let spawned = spawn_verified(&ff, &probe, &mut params, &pattern, Some(10), None)
            .await
            .map_err(|e| {
                self.note(&e);
                e
            })?;
        if params.encoder != first_name {
            if params.encoder == "libx264" || params.encoder == "libx265" {
                self.note("Hardware encoder unavailable. Using software encoding.");
            } else {
                self.note(&format!(
                    "{first_label} unavailable. Using {}.",
                    params.encoder_label
                ));
            }
        } else if wanted_audio != "none" && params.audio == "none" {
            self.note("Audio capture failed — replay video only.");
        }
        let Spawned { child, live } = spawned;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let duration = s.replay_duration.clamp(5, 600);
        let supervisor = {
            let flag = stop_flag.clone();
            let dir = segdir.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    prune_segments(&dir, duration);
                }
            })
        };
        self.replay = Some(ActiveReplay {
            child: Some(child),
            segdir,
            started_at: Instant::now(),
            duration_s: duration,
            params,
            live,
            stop_flag,
            supervisor: Some(supervisor),
        });
        self.apply_tray(app);
        self.notify(
            app,
            "Instant Replay",
            &format!("Buffering last {duration}s. Press Ctrl+Shift+I to save."),
        );
        Ok(status_of(self))
    }

    pub async fn replay_stop(&mut self, app: &tauri::AppHandle) -> Result<RecStatus, String> {
        let mut rp = self.replay.take().ok_or("Instant Replay is off.".to_string())?;
        rp.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = rp.supervisor.take() {
            h.abort();
        }
        if let Some(mut child) = rp.child.take() {
            quit_child(&mut child).await;
        }
        let _ = std::fs::remove_dir_all(&rp.segdir);
        self.apply_tray(app);
        Ok(status_of(self))
    }

    /// Save the last N seconds as a final MP4. Buffer keeps rolling afterwards.
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
            return Err("Instant Replay is not ready yet. Please wait a few seconds. / انتظر بضع ثوانٍ حتى يجهز.".to_string());
        }
        // Pause the rolling buffer while finalizing, then resume it.
        let mut rp = self.replay.take().unwrap();
        rp.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = rp.supervisor.take() {
            h.abort();
        }
        if let Some(mut child) = rp.child.take() {
            quit_child(&mut child).await;
        }
        let result = self.finalize_replay(app, &rp).await;
        // Resume buffering with a fresh segmenter (rolling continues).
        let resume = self.restart_replay_child(app, &rp.params, rp.duration_s).await;
        match (result, resume) {
            (Ok(_), Ok(_)) => {}
            (Err(e), _) => {
                // Buffer is already restarted if resume ok; surface save error.
                self.note(&e);
                self.apply_tray(app);
                return Err(e);
            }
            (_, Err(e)) => {
                self.note(&format!("Replay saved, but buffer restart failed: {e}"));
            }
        }
        self.apply_tray(app);
        Ok(status_of(self))
    }

    async fn restart_replay_child(
        &mut self,
        app: &tauri::AppHandle,
        params: &Params,
        duration: u64,
    ) -> Result<(), String> {
        let ff = ffmpeg_path().ok_or("Recorder engine missing.".to_string())?;
        let segdir = std::env::temp_dir().join("openscreen-replay");
        let _ = std::fs::remove_dir_all(&segdir);
        std::fs::create_dir_all(&segdir).map_err(|e| e.to_string())?;
        let pattern = segdir.join("seg%05d.mp4");
        let mut params = params.clone();
        let probe = get_probe(false)?;
        let spawned = spawn_verified(&ff, &probe, &mut params, &pattern, Some(10), None)
            .await
            .map_err(|e| {
                self.note(&e);
                e
            })?;
        let Spawned { child, live } = spawned;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let supervisor = {
            let flag = stop_flag.clone();
            let dir = segdir.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    prune_segments(&dir, duration);
                }
            })
        };
        self.replay = Some(ActiveReplay {
            child: Some(child),
            segdir,
            started_at: Instant::now(),
            duration_s: duration,
            params: params.clone(),
            live,
            stop_flag,
            supervisor: Some(supervisor),
        });
        let _ = app;
        Ok(())
    }

    async fn finalize_replay(
        &mut self,
        app: &tauri::AppHandle,
        rp: &ActiveReplay,
    ) -> Result<PathBuf, String> {
        let cutoff = std::time::SystemTime::now()
            - Duration::from_secs(rp.duration_s + 12); // segment granularity margin
        let mut segs: Vec<(PathBuf, std::time::SystemTime)> = vec![];
        if let Ok(rd) = std::fs::read_dir(&rp.segdir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("mp4") {
                    continue;
                }
                let mt = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if mt >= cutoff {
                    segs.push((p, mt));
                }
            }
        }
        segs.sort_by(|a, b| a.0.cmp(&b.0));
        let files: Vec<PathBuf> = segs.into_iter().map(|(p, _)| p).collect();
        if files.is_empty() {
            return Err("Instant Replay is not ready yet. Please wait a few seconds.".to_string());
        }
        let s = settings::load_settings();
        let folder = default_folder(&s);
        let _ = std::fs::create_dir_all(&folder);
        let est: u64 = files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        check_disk(&folder, est + 100 * 1024 * 1024)?;
        let out = folder.join(final_name(true));
        let ff = ffmpeg_path().ok_or("Recorder engine missing.".to_string())?;
        concat_copy(&ff, &files, &out).await?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let item = RecHistoryItem {
            id: uuid::Uuid::new_v4().to_string(),
            name: out
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("replay.mp4")
                .to_string(),
            path: out.to_string_lossy().to_string(),
            created_at: chrono::Local::now().to_rfc3339(),
            duration_s: rp.duration_s.min(
                rp.started_at.elapsed().as_secs(),
            ),
            width: rp.params.out_w,
            height: rp.params.out_h,
            fps: rp.params.fps,
            size,
            kind: "replay".to_string(),
        };
        rec_history_add(item, s.rec_history_limit);
        self.notify(app, "Instant Replay saved", &out.to_string_lossy());
        apply_post_action(app, &s.replay_post_action, &out);
        Ok(out)
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
fn tooltip_text(rec: &Recorder) -> String {
    if let Some(r) = &rec.rec {
        let mut ms = r.active_ms;
        if let Some(t) = r.active_start {
            ms += t.elapsed().as_millis() as u64;
        }
        let s = ms / 1000;
        if r.child.is_some() {
            format!(
                "● Recording {:02}:{:02} — Ctrl+Shift+R to stop",
                s / 60,
                s % 60
            )
        } else {
            format!("❚❚ Paused {:02}:{:02}", s / 60, s % 60)
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

fn prune_segments(dir: &Path, duration_s: u64) {
    let cutoff =
        std::time::SystemTime::now() - Duration::from_secs(duration_s + 25); // keep margin
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("mp4") {
                continue;
            }
            if let Ok(m) = e.metadata() {
                if let Ok(mt) = m.modified() {
                    if mt < cutoff {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }
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
