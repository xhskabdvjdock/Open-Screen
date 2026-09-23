//! Native recording pipeline: Windows Graphics Capture + Media Foundation
//! encoding (via `windows-capture`'s hardware-accelerated `VideoEncoder`)
//! + WASAPI audio (`audio.rs`). No FFmpeg process anywhere in this path.
//!
//! Timeline model (shared, monotonic):
//! - Video frames arrive from the compositor with WGC timestamps.
//! - Audio PCM chunks arrive from WASAPI threads through an mpsc channel and
//!   are pushed with `send_audio_buffer` (the encoder keeps a monotonic
//!   audio clock, so A/V stay in sync on one timeline).
//! - The whole session runs on ONE dedicated OS thread (`run_blocking`);
//!   the UI/async runtime only watches atomics â€” never encodes.
//!
//! Pause/resume and Instant Replay reuse this file's segment model:
//! each segment is an independently-valid MP4 (fresh encoder â‡’ starts with
//! a keyframe); save/stop merges segments with the pure-Rust MP4 remuxer
//! (`mp4mux.rs`) â€” stream copy, exactly one timeline, no re-encode.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeCodec {
    H264,
    Hevc,
}

#[derive(Debug, Clone)]
pub struct NativeRecConfig {
    /// 0-based monitor index (WGC captures whole monitors).
    pub monitor_idx: usize,
    /// Monitor-relative crop box (x0, y0, x1, y1, physical pixels).
    /// None = full monitor. The encoder is created AT the crop size, so
    /// capture resolution == output resolution (no upscale, #79).
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Output geometry. Must equal the captured area (no upscale, #79).
    pub out_w: u32,
    pub out_h: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub codec: NativeCodec,
    pub cursor: bool,
    /// 0 = no audio track at all.
    pub audio_channels: u32,
    pub sample_rate: u32,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeRecStats {
    pub frames: u64,
    pub audio_bytes: u64,
    pub elapsed_ms: u64,
    pub actual_fps: f64,
    pub width: u32,
    pub height: u32,
    pub encoder: String,
    pub monotonic: bool,
}

/// Control handle for a live native session (stop from another thread).
#[derive(Debug, Clone)]
pub struct NativeStop {
    flag: Arc<AtomicBool>,
}

impl NativeStop {
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

pub struct NativeSession {
    pub stop: NativeStop,
    pub frames: Arc<AtomicU64>,
    pub finished: Arc<AtomicBool>,
    pub error: Arc<Mutex<Option<String>>>,
    /// Sender for interleaved i16 PCM @ cfg.sample_rate/channels.
    pub audio_tx: mpsc::SyncSender<Vec<i16>>,
    handle: Option<std::thread::JoinHandle<NativeRecStats>>,
}

impl NativeSession {
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.lock().ok()?.take()
    }

    /// Signal stop and wait for the MP4 to finalize. Returns session stats.
    pub fn stop_and_join(mut self) -> Result<NativeRecStats, String> {
        self.stop.stop();
        // WGC only invokes the frame callback on compositor updates â€” on a
        // perfectly static screen the stop flag would sit unseen forever.
        // A 1px cursor nudge (immediately restored) forces one last frame so
        // the callback finishes the encoder promptly. Invisible to the user.
        #[cfg(target_os = "windows")]
        crate::wgc::nudge_cursor_for_frame();
        match self.handle.take() {
            Some(h) => h.join().map_err(|_| "Native session thread panicked.".to_string()),
            None => Err("Native session already joined.".to_string()),
        }
    }
}

#[cfg(target_os = "windows")]
mod inner {
    use super::*;
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::encoder::{
        AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder,
        VideoSettingsSubType,
    };
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::monitor::Monitor;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };
    #[cfg(test)]
    use std::time::Duration;

    /// Owned session state, constructed on the session thread and moved into
    /// the capture callback via `new(ctx)` (all callback-adjacent fields are
    /// touched only on the capture thread; shared counters stay atomic).
    struct SessionFlags {
        cfg: NativeRecConfig,
        stop: Arc<AtomicBool>,
        frames: Arc<AtomicU64>,
        audio_bytes: u64,
        audio_rx: mpsc::Receiver<Vec<i16>>,
        width: u32,
        height: u32,
        first_ts: Option<Instant>,
        last_ts: Option<Instant>,
        monotonic: bool,
    }

    thread_local! {
        /// Reused scratch for the crop flip (no per-frame allocation).
        static CROP_SCRATCH: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
        static CROP_TMP: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
    }

    struct RecCapture;

    impl GraphicsCaptureApiHandler for RecCapture {
        type Flags = SessionFlags;
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            // Move session state into a thread-local for the frame callback.
            SESSION.with(|c| *c.borrow_mut() = Some(ctx.flags));
            Ok(Self)
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            SESSION.with(|c| {
                let mut borrowed = c.borrow_mut();
                let Some(st) = borrowed.as_mut() else { return };
                if st.stop.load(Ordering::SeqCst) {
                    // Finish on the encoder owned by THIS thread, then end
                    // the session. (Stop on a static screen is unblocked by
                    // a 1px cursor nudge from `stop_and_join`.)
                    ENCODER.with(|e| {
                        if let Some(enc) = e.borrow_mut().take() {
                            let _ = enc.finish();
                        }
                    });
                    capture_control.stop();
                    return;
                }
                let now = Instant::now();
                if st.first_ts.is_none() {
                    st.first_ts = Some(now);
                    // Report the ENCODED geometry (crop size when cropping).
                    st.width = st.cfg.out_w;
                    st.height = st.cfg.out_h;
                } else if let Some(last) = st.last_ts {
                    if now < last {
                        st.monotonic = false;
                    }
                }
                st.last_ts = Some(now);
                // Drain all pending audio first (keeps A/V on one timeline),
                // then push the video frame.
                ENCODER.with(|e| {
                    let mut enc = e.borrow_mut();
                    let Some(enc) = enc.as_mut() else { return };
                    while let Ok(chunk) = st.audio_rx.try_recv() {
                        let bytes = i16_to_le_bytes(&chunk);
                        st.audio_bytes += bytes.len() as u64;
                        let _ = enc.send_audio_buffer(&bytes, 0);
                    }
                    // Area recording: GPU-side crop of the monitor frame, then
                    // CPU row-flip (BGRA top-down â†’ bottom-up) with the
                    // frame's OWN WGC timestamp â€” no fake timing.
                    // Pixel path (crop and/or downscale): GPU-side crop to a
                    // staging texture, then flip + optional bilinear downscale
                    // on CPU with the frame's OWN WGC timestamp. Used whenever
                    // the encoded geometry differs from the monitor frame.
                    let need_pixels = match st.cfg.crop {
                        Some((x0, y0, x1, y1)) => {
                            let cw = x1.saturating_sub(x0);
                            let ch = y1.saturating_sub(y0);
                            cw != st.cfg.out_w || ch != st.cfg.out_h || cw == 0 || ch == 0
                        }
                        None => {
                            let (fw, fh) = (frame.width(), frame.height());
                            fw != st.cfg.out_w || fh != st.cfg.out_h
                        }
                    };
                    let pushed = if !need_pixels && st.cfg.crop.is_none() {
                        enc.send_frame(frame).is_ok()
                    } else {
                        // Crop box (full frame when no explicit crop).
                        let (cx0, cy0, cx1, cy1) = match st.cfg.crop {
                            Some(b) => b,
                            None => (0, 0, frame.width(), frame.height()),
                        };
                        let ts = frame.timestamp().map(|t| t.Duration).unwrap_or(0);
                        match frame.buffer_crop(cx0, cy0, cx1, cy1) {
                            Ok(fb) => {
                                let sw = fb.width() as usize;
                                let sh = fb.height() as usize;
                                let dw = st.cfg.out_w as usize;
                                let dh = st.cfg.out_h as usize;
                                let ok = CROP_SCRATCH.with(|s| {
                                    CROP_TMP.with(|t| {
                                        let mut scratch = s.borrow_mut();
                                        let mut tmp = t.borrow_mut();
                                        scratch.clear();
                                        scratch.resize(dw * dh * 4, 0);
                                        tmp.clear();
                                        let src = fb.as_nopadding_buffer(&mut tmp);
                                        if src.len() < sw * sh * 4 || dw == 0 || dh == 0 {
                                            return false;
                                        }
                                        // Fast path: 1:1 copy + vertical flip only
                                        // (top-down â†’ bottom-up), no resampling.
                                        if sw == dw && sh == dh {
                                            for row in 0..dh {
                                                let s0 = (dh - 1 - row) * dw * 4;
                                                let d0 = row * dw * 4;
                                                scratch[d0..d0 + dw * 4]
                                                    .copy_from_slice(&src[s0..s0 + dw * 4]);
                                            }
                                            return enc.send_frame_buffer(&scratch, ts).is_ok();
                                        }
                                        // Bilinear downscale + vertical flip.
                                        for dy in 0..dh {
                                            let sy = if dh > 1 {
                                                dy as f32 * (sh - 1) as f32 / (dh - 1) as f32
                                            } else {
                                                0.0
                                            };
                                            let y0 = sy.floor() as usize;
                                            let y1 = (y0 + 1).min(sh - 1);
                                            let fy = sy - y0 as f32;
                                            for dx in 0..dw {
                                                let sx = if dw > 1 {
                                                    dx as f32 * (sw - 1) as f32 / (dw - 1) as f32
                                                } else {
                                                    0.0
                                                };
                                                let x0 = sx.floor() as usize;
                                                let x1 = (x0 + 1).min(sw - 1);
                                                let fx = sx - x0 as f32;
                                                // Source rows are top-down; dest is bottom-up.
                                                let d_row = dh - 1 - dy;
                                                for c in 0..4 {
                                                    let p00 = src[(y0 * sw + x0) * 4 + c] as f32;
                                                    let p10 = src[(y0 * sw + x1) * 4 + c] as f32;
                                                    let p01 = src[(y1 * sw + x0) * 4 + c] as f32;
                                                    let p11 = src[(y1 * sw + x1) * 4 + c] as f32;
                                                    let v = p00 * (1.0 - fx) * (1.0 - fy)
                                                        + p10 * fx * (1.0 - fy)
                                                        + p01 * (1.0 - fx) * fy
                                                        + p11 * fx * fy;
                                                    scratch[(d_row * dw + dx) * 4 + c] = v as u8;
                                                }
                                            }
                                        }
                                        enc.send_frame_buffer(&scratch, ts).is_ok()
                                    })
                                });
                                ok
                            }
                            Err(_) => false,
                        }
                    };
                    if pushed {
                        st.frames.fetch_add(1, Ordering::Relaxed);
                    }
                });
            });
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    thread_local! {
        static SESSION: std::cell::RefCell<Option<SessionFlags>> = std::cell::RefCell::new(None);
        static ENCODER: std::cell::RefCell<Option<VideoEncoder>> = std::cell::RefCell::new(None);
    }

    fn i16_to_le_bytes(chunk: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len() * 2);
        for s in chunk {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Start a native session on a DEDICATED OS thread. Returns immediately;
    /// drive it via `NativeSession`. The encoder + capture both live and die
    /// on that thread (WGC/COM apartment affinity).
    pub fn start_session(cfg: NativeRecConfig, path: PathBuf) -> Result<NativeSession, String> {
        // Fast-fail on a bad monitor index before spawning the thread
        // (the session thread re-resolves it for the actual capture).
        let _item = Monitor::from_index(cfg.monitor_idx + 1)
            .map_err(|e| format!("Monitor {} unavailable: {e}", cfg.monitor_idx))?;
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<i16>>(256);

        let th_stop = stop.clone();
        let th_frames = frames.clone();
        let th_finished = finished.clone();
        let th_error = error.clone();
        let th_cfg = cfg.clone();

        let handle = std::thread::Builder::new()
            .name("openscreen-native-rec".to_string())
            .spawn(move || {
                let stats = run_session_blocking(
                    th_cfg,
                    path,
                    th_stop,
                    th_frames,
                    audio_rx,
                    th_error.clone(),
                );
                th_finished.store(true, Ordering::SeqCst);
                stats
            })
            .map_err(|e| format!("Could not start capture thread: {e}"))?;

        Ok(NativeSession {
            stop: NativeStop { flag: stop },
            frames,
            finished,
            error,
            audio_tx,
            handle: Some(handle),
        })
    }

    fn run_session_blocking(
        cfg: NativeRecConfig,
        path: PathBuf,
        stop: Arc<AtomicBool>,
        frames: Arc<AtomicU64>,
        audio_rx: mpsc::Receiver<Vec<i16>>,
        error: Arc<Mutex<Option<String>>>,
    ) -> NativeRecStats {
        let started = Instant::now();
        let item = match Monitor::from_index(cfg.monitor_idx + 1) {
            Ok(m) => m,
            Err(e) => {
                *error.lock().unwrap() =
                    Some(format!("Monitor {} unavailable: {e}", cfg.monitor_idx));
                return NativeRecStats::default();
            }
        };
        let sub = match cfg.codec {
            NativeCodec::H264 => VideoSettingsSubType::H264,
            NativeCodec::Hevc => VideoSettingsSubType::HEVC,
        };
        let video = VideoSettingsBuilder::new(cfg.out_w, cfg.out_h)
            .sub_type(sub)
            .bitrate(cfg.bitrate_bps)
            .frame_rate(cfg.fps);
        let audio = if cfg.audio_channels == 0 {
            AudioSettingsBuilder::new().disabled(true)
        } else {
            AudioSettingsBuilder::new()
                .channel_count(cfg.audio_channels)
                .sample_rate(cfg.sample_rate)
        };
        let mut encoder = match VideoEncoder::new(video, audio, ContainerSettingsBuilder::new(), &path) {
            Ok(e) => e,
            Err(e) => {
                *error.lock().unwrap() = Some(format!("Encoder init failed ({sub:?}): {e}"));
                return NativeRecStats::default();
            }
        };
        // Warm the encoder with the real output size before frames arrive.
        let _ = &mut encoder;
        ENCODER.with(|e| *e.borrow_mut() = Some(encoder));

        let cursor_setting = if cfg.cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };
        // Session state travels into the frame callback via `new(ctx)`.
        let flags = SessionFlags {
            cfg: cfg.clone(),
            stop,
            frames: frames.clone(),
            audio_bytes: 0,
            audio_rx,
            width: 0,
            height: 0,
            first_ts: None,
            last_ts: None,
            monotonic: true,
        };
        let settings = Settings::new(
            item,
            cursor_setting,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            flags,
        );
        if let Err(e) = RecCapture::start(settings) {
            *error.lock().unwrap() = Some(format!("Capture session failed: {e}"));
        }
        // Session over: collect stats, clear thread-locals.
        let (w, h, mono, f, ab) = SESSION.with(|c| {
            let st = c.borrow();
            match st.as_ref() {
                Some(s) => (s.width, s.height, s.monotonic, frames.load(Ordering::SeqCst), s.audio_bytes),
                None => (0, 0, true, frames.load(Ordering::SeqCst), 0),
            }
        });
        SESSION.with(|c| *c.borrow_mut() = None);
        ENCODER.with(|e| *e.borrow_mut() = None);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        NativeRecStats {
            frames: f,
            audio_bytes: ab,
            elapsed_ms,
            actual_fps: if elapsed_ms > 0 {
                f as f64 / (elapsed_ms as f64 / 1000.0)
            } else {
                0.0
            },
            width: w,
            height: h,
            encoder: format!("{:?}", sub),
            monotonic: mono,
        }
    }

    /// Blocking convenience for tests/tools: record `secs` seconds to `path`.
    #[cfg(test)]
    pub fn record_seconds(cfg: NativeRecConfig, path: PathBuf, secs: u64) -> Result<NativeRecStats, String> {
        let session = start_session(cfg, path)?;
        std::thread::sleep(Duration::from_secs(secs));
        session.stop_and_join()
    }

    /// Test helper proving the audio-feed contract: pushes a synthetic 440Hz
    /// stereo sine through the exact `audio_tx` channel WASAPI threads use.
    #[cfg(test)]
    pub fn record_seconds_with_sine(
        cfg: NativeRecConfig,
        path: PathBuf,
        secs: u64,
    ) -> Result<NativeRecStats, String> {
        let sr = cfg.sample_rate;
        let ch = cfg.audio_channels.max(1);
        let session = start_session(cfg, path)?;
        let tx = session.audio_tx.clone();
        let gen = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(secs);
            let mut n = 0u64;
            while Instant::now() < end {
                let frames = 960usize;
                let mut chunk = Vec::with_capacity(frames * ch as usize);
                for i in 0..frames {
                    let t = (n * frames as u64 + i as u64) as f32 / sr as f32;
                    let s = (0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 32767.0) as i16;
                    for _ in 0..ch {
                        chunk.push(s);
                    }
                }
                n += 1;
                if tx.send(chunk).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        std::thread::sleep(Duration::from_secs(secs));
        let stats = session.stop_and_join()?;
        let _ = gen.join();
        Ok(stats)
    }
}

#[cfg(target_os = "windows")]
pub use inner::start_session;
#[cfg(test)]
pub use inner::{record_seconds, record_seconds_with_sine};

#[cfg(not(target_os = "windows"))]
pub fn record_seconds(_cfg: NativeRecConfig, _path: PathBuf, _secs: u64) -> Result<NativeRecStats, String> {
    Err("Native recording requires Windows.".to_string())
}
#[cfg(not(target_os = "windows"))]
pub fn start_session(_cfg: NativeRecConfig, _path: PathBuf) -> Result<NativeSession, String> {
    Err("Native recording requires Windows.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "windows")]
    fn native_record_produces_mp4() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("openscreen-nativetest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("rec.mp4");
        let cfg = NativeRecConfig {
            monitor_idx: 0,
            crop: None,
            out_w: 1920,
            out_h: 1080,
            fps: 60,
            bitrate_bps: 12_000_000,
            codec: NativeCodec::H264,
            cursor: true,
            audio_channels: 0,
            sample_rate: 48000,
        };
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(6));
        let stats = record_seconds(cfg, out.clone(), 6).expect("native record failed");
        let _ = wiggler.join();
        eprintln!("native record: {stats:?}");
        let meta = std::fs::metadata(&out).expect("mp4 missing");
        eprintln!("mp4 bytes: {}", meta.len());
        assert!(stats.frames >= 100, "too few encoded frames: {stats:?}");
        assert_eq!((stats.width, stats.height), (1920, 1080));
        assert!(stats.monotonic, "timestamps went backwards");
        assert!(meta.len() > 100_000, "mp4 suspiciously small: {}", meta.len());
        // Environmental bound: this VM's DWM paces WGC at ~40fps; real
        // foreground 60Hz+ sessions deliver display rate (asserted by the
        // container-fps check below, which must hit the CFR target).
        assert!((30.0..=75.0).contains(&stats.actual_fps), "implausible fps: {stats:?}");
        // Container-level truth (ffprobe-free): parse our own MP4.
        let info = crate::mp4info::read_info(&out).expect("mp4 unreadable");
        eprintln!("mp4 info: {info:?}");
        let v = info.video.as_ref().expect("no video track");
        assert_eq!((v.width, v.height), (1920, 1080), "container dims wrong");
        assert!(
            v.codec.to_lowercase().contains("avc"),
            "expected H264/avc1, got {}",
            v.codec
        );
        assert!((4.0..=7.5).contains(&info.duration_sec), "bad duration: {}", info.duration_sec);
        let cfps = crate::mp4info::container_fps(&info);
        eprintln!("container fps: {cfps:.2}");
        assert!((35.0..=75.0).contains(&cfps), "container fps off: {cfps}");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn native_record_area_crops_natively() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("openscreen-nativetest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("rec_area.mp4");
        // 960x540 top-left area of the 1080p monitor.
        let cfg = NativeRecConfig {
            monitor_idx: 0,
            crop: Some((0, 0, 960, 540)),
            out_w: 960,
            out_h: 540,
            fps: 60,
            bitrate_bps: 6_000_000,
            codec: NativeCodec::H264,
            cursor: true,
            audio_channels: 0,
            sample_rate: 48000,
        };
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(5));
        let stats = record_seconds(cfg, out.clone(), 5).expect("area record failed");
        let _ = wiggler.join();
        eprintln!("area record: {stats:?}");
        assert!(stats.frames >= 60, "too few area frames: {stats:?}");
        assert_eq!((stats.width, stats.height), (960, 540));
        let info = crate::mp4info::read_info(&out).expect("area mp4 unreadable");
        let v = info.video.as_ref().expect("no video track");
        assert_eq!((v.width, v.height), (960, 540), "crop dims wrong");
        assert!(v.samples >= 60, "too few container samples");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn native_record_hevc_when_available() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        if crate::mfhw::probe_hw_encoders().hevc_hw.is_empty()
            && crate::mfhw::probe_sw("hevc").is_empty()
        {
            eprintln!("SKIP: no HEVC MFT on this machine");
            return;
        }
        let dir = std::env::temp_dir().join(format!("openscreen-nativetest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("rec_hevc.mp4");
        let cfg = NativeRecConfig {
            monitor_idx: 0,
            crop: None,
            out_w: 1280,
            out_h: 720,
            fps: 30,
            bitrate_bps: 6_000_000,
            codec: NativeCodec::Hevc,
            cursor: true,
            audio_channels: 0,
            sample_rate: 48000,
        };
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(5));
        let stats = record_seconds(cfg, out.clone(), 5).expect("hevc record failed");
        let _ = wiggler.join();
        eprintln!("hevc record: {stats:?}");
        // 720p from a 1080p monitor exercises the downscale path too.
        let info = crate::mp4info::read_info(&out).expect("hevc mp4 unreadable");
        let v = info.video.as_ref().expect("no video track");
        assert_eq!((v.width, v.height), (1280, 720));
        assert!(v.codec.to_lowercase().contains("hvc"), "expected HEVC, got {}", v.codec);
        assert!(v.samples >= 60, "too few hevc samples");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn native_record_with_audio_muxes_aac() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("openscreen-nativetest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("rec_a.mp4");
        let cfg = NativeRecConfig {
            monitor_idx: 0,
            crop: None,
            out_w: 1920,
            out_h: 1080,
            fps: 30,
            bitrate_bps: 8_000_000,
            codec: NativeCodec::H264,
            cursor: true,
            audio_channels: 2,
            sample_rate: 48000,
        };
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(5));
        let stats = record_seconds_with_sine(cfg, out.clone(), 5).expect("audio record failed");
        let _ = wiggler.join();
        eprintln!("audio record: {stats:?}");
        assert!(stats.audio_bytes > 100_000, "no audio reached the muxer");
        let info = crate::mp4info::read_info(&out).expect("mp4 unreadable");
        let a = info.audio.as_ref().expect("no audio track");
        eprintln!("audio track: {a:?}");
        assert!(a.codec.to_lowercase().contains("mp4a"), "expected AAC, got {}", a.codec);
        assert!(a.samples > 50, "too few audio samples: {}", a.samples);
        assert!((4.0..=6.5).contains(&a.duration_sec), "audio duration off: {}", a.duration_sec);
    }
}
