//! Windows-native screen capture foundation (no FFmpeg).
//!
//! - Capture: **Windows Graphics Capture** via the `windows-capture` crate
//!   (continuous frame pipeline driven by the compositor â€” never a
//!   screenshot loop, never `sleep(16ms)` pacing).
//! - Cursor: composited natively by WGC (`WithCursor` / `WithoutCursor`).
//! - Timestamps: `Instant::now()` (monotonic) per delivered frame.
//! - Encoding/muxing (recording + replay) build on this module + Media
//!   Foundation and reuse the existing WASAPI audio (`audio.rs`).

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WgcMonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WgcStats {
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub elapsed_ms: u64,
    pub actual_fps: f64,
    pub monotonic: bool,
}

#[cfg(target_os = "windows")]
mod inner {
    use super::*;
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::monitor::Monitor;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };

    #[derive(Debug, Default)]
    struct ProbeInner {
        frames: u64,
        width: u32,
        height: u32,
        first: Option<Instant>,
        last: Option<Instant>,
        monotonic: bool,
    }

    struct ProbeCapture;

    impl GraphicsCaptureApiHandler for ProbeCapture {
        type Flags = ();
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn new(_ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self)
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            // Flags are accessible through a fresh context? No â€” stash via
            // thread-local set by the runner before start.
            with_probe_state(|st| {
                let now = Instant::now();
                let mut s = st.stats.lock().unwrap();
                if s.frames == 0 {
                    s.width = frame.width();
                    s.height = frame.height();
                    s.first = Some(now);
                    s.monotonic = true;
                } else if let Some(last) = s.last {
                    if now < last {
                        s.monotonic = false;
                    }
                }
                s.frames += 1;
                s.last = Some(now);
                if now >= st.deadline {
                    let _ = capture_control.stop();
                }
            });
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    struct ProbeState {
        deadline: Instant,
        stats: Arc<Mutex<ProbeInner>>,
    }

    thread_local! {
        static PROBE_STATE: std::cell::RefCell<Option<ProbeState>> = std::cell::RefCell::new(None);
    }

    fn with_probe_state(f: impl FnOnce(&ProbeState)) {
        PROBE_STATE.with(|c| {
            if let Some(st) = c.borrow().as_ref() {
                f(st);
            }
        });
    }

    pub fn backend_name() -> &'static str {
        "Windows Graphics Capture"
    }

    pub fn list_monitors() -> Result<Vec<WgcMonitorInfo>, String> {
        let mons = Monitor::enumerate().map_err(|e| format!("Monitor enumerate failed: {e}"))?;
        let primary_name = Monitor::primary()
            .ok()
            .and_then(|p| p.device_name().ok())
            .unwrap_or_default();
        let mut out = vec![];
        for (i, m) in mons.iter().enumerate() {
            let name = m.device_name().unwrap_or_else(|_| format!("Display {}", i + 1));
            out.push(WgcMonitorInfo {
                index: i,
                name: name.clone(),
                width: m.width().unwrap_or(0),
                height: m.height().unwrap_or(0),
                refresh_rate: m.refresh_rate().unwrap_or(0),
                primary: name == primary_name,
            });
        }
        Ok(out)
    }

    pub fn probe_primary_monitor(
        secs: u64,
        cursor: bool,
        item_override: Option<usize>,
    ) -> Result<WgcStats, String> {
        // `Monitor` converts directly into a WGC capture item (monitor-level
        // capture needs no picker dialog and no window handle).
        let item = match item_override {
            Some(idx) => Monitor::from_index(idx + 1)
                .map_err(|e| format!("Monitor {idx} not found: {e}"))?,
            None => Monitor::primary()
                .map_err(|e| format!("Primary monitor not found: {e}"))?,
        };

        let stats = Arc::new(Mutex::new(ProbeInner::default()));
        let deadline = Instant::now() + Duration::from_secs(secs.max(1));
        PROBE_STATE.with(|c| {
            *c.borrow_mut() = Some(ProbeState {
                deadline,
                stats: stats.clone(),
            });
        });
        let cursor_setting = if cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };
        let settings = Settings::new(
            item,
            cursor_setting,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            (),
        );
        let started = Instant::now();
        ProbeCapture::start(settings).map_err(|e| format!("WGC session failed: {e}"))?;
        PROBE_STATE.with(|c| *c.borrow_mut() = None);
        let s = stats.lock().unwrap();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let actual_fps = if elapsed_ms > 0 {
            s.frames as f64 / (elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        Ok(WgcStats {
            backend: backend_name().to_string(),
            width: s.width,
            height: s.height,
            frames: s.frames,
            elapsed_ms,
            actual_fps,
            monotonic: s.monotonic,
        })
    }

    /// Move the cursor by 1px and back to force exactly one compositor
    /// update (used to unblock session teardown on a static screen).
    pub fn nudge_cursor_for_frame() {
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        unsafe {
            let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
            if GetCursorPos(&mut pt).is_ok() {
                let _ = SetCursorPos(pt.x + 1, pt.y);
                let _ = SetCursorPos(pt.x, pt.y);
            }
        }
    }

    /// Wiggle the system cursor in a small circle to force compositor updates
    /// (WGC only delivers frames on content change). Restores position after.
    /// Test/diagnostic helper only.
    #[cfg(test)]
    pub fn wiggle_cursor(seconds: u64) {
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        unsafe {
            if GetCursorPos(&mut pt).is_err() {
                return;
            }
        }
        let (cx, cy) = (pt.x, pt.y);
        let end = Instant::now() + Duration::from_secs(seconds);
        let mut t = 0.0f64;
        while Instant::now() < end {
            t += 0.35;
            let x = cx + (30.0 * t.cos()) as i32;
            let y = cy + (30.0 * t.sin()) as i32;
            unsafe {
                let _ = SetCursorPos(x, y);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        unsafe {
            let _ = SetCursorPos(cx, cy);
        }
    }
}

#[cfg(target_os = "windows")]
pub use inner::{backend_name, list_monitors, nudge_cursor_for_frame, probe_primary_monitor};
#[cfg(test)]
pub use inner::wiggle_cursor;

#[cfg(not(target_os = "windows"))]
pub fn backend_name() -> &'static str {
    "unsupported"
}
#[cfg(not(target_os = "windows"))]
pub fn list_monitors() -> Result<Vec<WgcMonitorInfo>, String> {
    Err("Windows Graphics Capture requires Windows.".to_string())
}
#[cfg(not(target_os = "windows"))]
pub fn probe_primary_monitor(_secs: u64, _cursor: bool, _item: Option<usize>) -> Result<WgcStats, String> {
    Err("Windows Graphics Capture requires Windows.".to_string())
}
#[cfg(not(target_os = "windows"))]
pub fn wiggle_cursor(_seconds: u64) {}

/// Global capture mutex: the machine has ONE cursor and ONE compositor.
/// Capture tests must hold this for their whole run â€” parallel captures
/// would fight over the cursor and trash each other's frame timing.
pub static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "windows")]
    fn wgc_monitor_specs() {
        let mons = list_monitors().expect("enumerate failed");
        eprintln!("monitors: {mons:?}");
        assert!(!mons.is_empty());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn wgc_sustained_delivery_rate() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        // Drive content hard (4ms cursor steps) and measure what the
        // compositor actually delivers. Honest environmental bound: this VM's
        // DWM paces WGC at ~43fps; real 60/120Hz foreground sessions deliver
        // display rate. The pipeline must REPORT the truth either way.
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let wiggler = std::thread::spawn(|| {
            let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
            unsafe {
                if GetCursorPos(&mut pt).is_err() {
                    return;
                }
            }
            let (cx, cy) = (pt.x, pt.y);
            let end = std::time::Instant::now() + std::time::Duration::from_secs(4);
            let mut t = 0.0f64;
            while std::time::Instant::now() < end {
                t += 0.9;
                unsafe {
                    let _ = SetCursorPos(cx + (40.0 * t.cos()) as i32, cy + (40.0 * t.sin()) as i32);
                }
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
            unsafe {
                let _ = SetCursorPos(cx, cy);
            }
        });
        let stats = probe_primary_monitor(4, true, None).expect("WGC capture failed");
        let _ = wiggler.join();
        eprintln!("delivery probe: {stats:?}");
        assert!(stats.frames >= 100, "delivery stalled: {stats:?}");
        assert!(stats.monotonic, "timestamps went backwards");
        assert!((20.0..=130.0).contains(&stats.actual_fps), "implausible rate: {stats:?}");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn wgc_probe_delivers_timestamped_frames() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        // Force compositor updates (cursor motion exercises the cursor path).
        let wiggler = std::thread::spawn(|| wiggle_cursor(4));
        let stats = probe_primary_monitor(4, true, None).expect("WGC capture failed");
        let _ = wiggler.join();
        eprintln!("WGC probe: {stats:?}");
        assert_eq!(stats.backend, "Windows Graphics Capture");
        assert!(stats.width >= 320 && stats.height >= 200, "bad size: {:?}", stats);
        assert!(stats.frames >= 30, "too few frames: {:?}", stats);
        assert!(stats.monotonic, "timestamps went backwards");
        // Display-paced capture: allow wide honest bounds (30/60/120Hz+).
        assert!(
            (10.0..=130.0).contains(&stats.actual_fps),
            "implausible fps: {:?}",
            stats
        );
    }
}
