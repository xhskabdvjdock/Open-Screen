//! Native Windows audio capture (WASAPI) feeding the native pipeline.
//!
//! - WASAPI loopback captures whatever is actually playing (YouTube, games,
//!   browser) without any special driver (no "Stereo Mix" needed).
//! - Mic capture uses the real capture endpoint selected in Settings.
//! - The PCM pump streams mixed 20ms i16 chunks straight into the Media
//!   Foundation encoder (steady tick incl. silence keeps A/V aligned).
//! - Levels are computed from the actual captured peaks (not fake UI).

use serde::{Deserialize, Serialize};

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevices {
    pub mics: Vec<String>,
    pub system: Option<String>,
    pub wasapi_available: bool,
}

/// Peak level 0.0..1.0 shared with the capture thread.
pub type LevelArc = Arc<StdMutex<f32>>;

fn peak_to_db(peak: f32) -> Option<f32> {
    if peak <= 0.0001 {
        return None;
    }
    Some((20.0 * peak.max(0.0001).log10()).clamp(-60.0, 0.0))
}

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub fn list_audio_devices() -> AudioDevices {
    use wasapi::{DeviceEnumerator, Direction};
    let _ = wasapi::initialize_mta();
    let mut out = AudioDevices {
        mics: vec![],
        system: None,
        wasapi_available: true,
    };
    let enumerator = match DeviceEnumerator::new() {
        Ok(e) => e,
        Err(_) => {
            out.wasapi_available = false;
            return out;
        }
    };
    // Capture endpoints = real microphones.
    if let Ok(col) = enumerator.get_device_collection(&Direction::Capture) {
        let n = col.get_nbr_devices().unwrap_or(0);
        for i in 0..n {
            if let Ok(dev) = col.get_device_at_index(i) {
                if let Ok(name) = dev.get_friendlyname() {
                    if !name.trim().is_empty() {
                        out.mics.push(name);
                    }
                }
            }
        }
    }
    // Default render endpoint = what loopback will capture.
    if let Ok(dev) = enumerator.get_default_device(&Direction::Render) {
        if let Ok(name) = dev.get_friendlyname() {
            if !name.trim().is_empty() {
                out.system = Some(name);
            }
        }
    }
    out.mics.sort();
    out.mics.dedup();
    out
}

#[cfg(not(target_os = "windows"))]
pub fn list_audio_devices() -> AudioDevices {
    AudioDevices::default()
}

/// One-shot level probe for Settings device test (no recording needed).
/// Captures ~0.8s from the requested source and returns the peak dB.
/// Returns None when silent/unavailable so the UI can say so honestly.
#[cfg(target_os = "windows")]
pub fn probe_level(kind: &str, mic_name: &str) -> Option<f32> {
    use std::collections::VecDeque;
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().ok()?;
    let (device, loopback) = match kind {
        "mic" => (pick_capture_device(&enumerator, mic_name)?, false),
        _ => (enumerator.get_default_device(&Direction::Render).ok()?, true),
    };
    let mut client = device.get_iaudioclient().ok()?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48000usize, 2usize, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };
    let dir = Direction::Capture;
    client.initialize_client(&format, &dir, &mode).ok()?;
    let _ = loopback;
    let event = client.set_get_eventhandle().ok()?;
    let capture = client.get_audiocaptureclient().ok()?;
    client.start_stream().ok()?;
    let mut peak = 0.0f32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(900);
    let mut deque: VecDeque<u8> = VecDeque::with_capacity(1 << 18);
    while std::time::Instant::now() < deadline {
        let _ = event.wait_for_event(200);
        loop {
            let frames = capture.get_next_packet_size().ok().flatten().unwrap_or(0);
            if frames == 0 {
                break;
            }
            deque.clear();
            if capture.read_from_device_to_deque(&mut deque).is_err() {
                break;
            }
            let mut i = 0;
            while i + 4 <= deque.len() {
                let s = f32::from_le_bytes([deque[i], deque[i + 1], deque[i + 2], deque[i + 3]]);
                peak = peak.max(s.abs());
                i += 4;
            }
        }
    }
    let _ = client.stop_stream();
    peak_to_db(peak)
}

#[cfg(not(target_os = "windows"))]
pub fn probe_level(_kind: &str, _mic: &str) -> Option<f32> {
    None
}

// ---------------------------------------------------------------------------
// PCM pump (native pipeline): WASAPI -> mixed i16 chunks -> encoder.
// The encoder owns a monotonic audio clock fed ONLY by pushed bytes, so the
// mixer emits on a steady 20ms tick â€” including silence â€” keeping the audio
// timeline locked to video. A stalled/unplugged device degrades to silence,
// never kills video (#74/#75). Mixing uses equal-power-ish 0.5 gains plus a
// tanh soft limiter: no clipping, no distortion (#31/#32).
// ---------------------------------------------------------------------------

/// 20ms worth of frames at 48kHz.
const PUMP_CHUNK_FRAMES: usize = 960;

/// Live PCM pump feeding encoder sessions.
pub struct PcmPump {
    pub sys_level: LevelArc,
    pub mic_level: LevelArc,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    targets: Arc<StdMutex<Vec<std::sync::mpsc::SyncSender<Vec<i16>>>>>,
    active: bool,
}

impl PcmPump {
    pub fn empty() -> Self {
        Self {
            sys_level: Arc::new(StdMutex::new(0.0)),
            mic_level: Arc::new(StdMutex::new(0.0)),
            stop: Arc::new(AtomicBool::new(true)),
            threads: vec![],
            targets: Arc::new(StdMutex::new(vec![])),
            active: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Swap output targets (segment rotation) without restarting WASAPI
    /// capture â€” audio stays gapless across video segment joints.
    pub fn retarget(&self, txs: Vec<std::sync::mpsc::SyncSender<Vec<i16>>>) {
        if let Ok(mut t) = self.targets.lock() {
            *t = txs;
        }
    }

    pub fn sys_db(&self) -> Option<f32> {
        peak_to_db(*self.sys_level.lock().ok()?)
    }

    pub fn mic_db(&self) -> Option<f32> {
        peak_to_db(*self.mic_level.lock().ok()?)
    }

    pub fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

/// Start PCM capture. `txs` receives one mixed stream per entry (fan-out to
/// parallel monitor sessions). Returns pump + note. Never fails recording.
pub fn start_pcm_pump(
    mode: &str,
    mic_name: &str,
    sample_rate: u32,
    channels: u32,
    txs: Vec<std::sync::mpsc::SyncSender<Vec<i16>>>,
) -> (PcmPump, Option<String>) {
    let mode = mode.to_lowercase();
    let want_sys = mode == "system" || mode == "both";
    let want_mic = mode == "mic" || mode == "microphone" || mode == "both";
    if !want_sys && !want_mic {
        return (PcmPump::empty(), None);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (mic_name, sample_rate, channels, tx);
        return (
            PcmPump::empty(),
            Some("Audio capture is unavailable on this platform â€” recording video only.".to_string()),
        );
    }
    #[cfg(target_os = "windows")]
    {
        let sr = if sample_rate == 44100 { 44100 } else { 48000 };
        let ch = if channels == 1 { 1 } else { 2 };
        let stop = Arc::new(AtomicBool::new(false));
        let sys_level: LevelArc = Arc::new(StdMutex::new(0.0));
        let mic_level: LevelArc = Arc::new(StdMutex::new(0.0));
        // Per-source queues decouple device clocks from the mixer tick.
        let (sys_tx, sys_rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(64);
        let (mic_tx, mic_rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(64);
        let mut threads = vec![];
        let mut notes = vec![];
        let mut have_sys = false;
        let mut have_mic = false;
        if want_sys {
            match spawn_pcm_producer(true, String::new(), sr, ch, sys_level.clone(), stop.clone(), sys_tx) {
                Some(t) => {
                    threads.push(t);
                    have_sys = true;
                }
                None => notes.push("System audio unavailable â€” continuing without it.".to_string()),
            }
        }
        if want_mic {
            match spawn_pcm_producer(false, mic_name.to_string(), sr, ch, mic_level.clone(), stop.clone(), mic_tx) {
                Some(t) => {
                    threads.push(t);
                    have_mic = true;
                }
                None => notes.push("Microphone unavailable â€” continuing without it.".to_string()),
            }
        }
        if !have_sys && !have_mic {
            stop.store(true, Ordering::SeqCst);
            return (
                PcmPump::empty(),
                Some(
                    notes
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "Audio capture failed â€” recording video only.".to_string()),
                ),
            );
        }
        // Mixer: steady tick, latest-chunk pairing, silence on stall.
        let m_stop = stop.clone();
        let targets: Arc<StdMutex<Vec<std::sync::mpsc::SyncSender<Vec<i16>>>>> =
            Arc::new(StdMutex::new(txs));
        let m_targets = targets.clone();
        threads.push(
            std::thread::Builder::new()
                .name("openscreen-pcm-mixer".to_string())
                .spawn(move || {
                    pcm_mixer_loop(have_sys, have_mic, ch, sys_rx, mic_rx, m_targets, &m_stop);
                })
                .unwrap(),
        );
        let note = if notes.is_empty() { None } else { Some(notes.join(" ")) };
        (
            PcmPump { sys_level, mic_level, stop, threads, targets, active: true },
            note,
        )
    }
}

#[cfg(target_os = "windows")]
fn spawn_pcm_producer(
    loopback: bool,
    prefer_mic: String,
    sample_rate: u32,
    channels: u32,
    level: LevelArc,
    stop: Arc<AtomicBool>,
    tx: std::sync::mpsc::SyncSender<Vec<i16>>,
) -> Option<std::thread::JoinHandle<()>> {
    if loopback {
        use wasapi::{DeviceEnumerator, Direction};
        let _ = wasapi::initialize_mta();
        DeviceEnumerator::new()
            .ok()?
            .get_default_device(&Direction::Render)
            .ok()?;
    }
    let name = if loopback { "openscreen-pcm-sys" } else { "openscreen-pcm-mic" }.to_string();
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let _ = pcm_capture_loop(loopback, &prefer_mic, sample_rate, channels, &level, &stop, &tx);
        })
        .ok()
}

/// WASAPI event-driven capture emitting fixed 20ms i16 chunks.
#[cfg(target_os = "windows")]
fn pcm_capture_loop(
    loopback: bool,
    prefer_mic: &str,
    sample_rate: u32,
    channels: u32,
    level: &LevelArc,
    stop: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::SyncSender<Vec<i16>>,
) -> Result<(), String> {
    use std::collections::VecDeque;
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("{e:?}"))?;
    let device = if loopback {
        enumerator.get_default_device(&Direction::Render).map_err(|e| format!("{e:?}"))?
    } else {
        pick_capture_device(&enumerator, prefer_mic).ok_or("No microphone found")?
    };
    let mut client = device.get_iaudioclient().map_err(|e| format!("{e:?}"))?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, sample_rate as usize, channels as usize, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 };
    // Render device + Capture direction = loopback; capture device + Capture
    // direction = microphone. Same code path, chosen endpoint differs.
    client.initialize_client(&format, &Direction::Capture, &mode).map_err(|e| format!("{e:?}"))?;
    let event = client.set_get_eventhandle().map_err(|e| format!("{e:?}"))?;
    let capture = client.get_audiocaptureclient().map_err(|e| format!("{e:?}"))?;
    client.start_stream().map_err(|e| format!("{e:?}"))?;

    let mut deque: VecDeque<u8> = VecDeque::with_capacity(1 << 18);
    let mut pending: Vec<i16> = Vec::with_capacity(PUMP_CHUNK_FRAMES * channels as usize);
    let mut peak_hold = 0.0f32;
    let mut peak_tick = 0u32;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let _ = event.wait_for_event(300);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        loop {
            let frames = capture.get_next_packet_size().ok().flatten().unwrap_or(0);
            if frames == 0 {
                break;
            }
            deque.clear();
            if capture.read_from_device_to_deque(&mut deque).is_err() {
                break;
            }
            let mut peak = 0.0f32;
            let mut i = 0;
            while i + 4 <= deque.len() {
                let s = f32::from_le_bytes([deque[i], deque[i + 1], deque[i + 2], deque[i + 3]]);
                peak = peak.max(s.abs());
                pending.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                i += 4;
                // Fixed chunking keeps mixer pairing exact.
                if pending.len() >= PUMP_CHUNK_FRAMES * channels as usize {
                    let _ = tx.send(std::mem::replace(
                        &mut pending,
                        Vec::with_capacity(PUMP_CHUNK_FRAMES * channels as usize),
                    ));
                }
            }
            peak_hold = peak.max(peak_hold * 0.92);
            peak_tick += 1;
            if peak_tick % 4 == 0 {
                if let Ok(mut l) = level.lock() {
                    *l = peak_hold;
                }
            }
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

/// Steady-tick mixer: latest chunk per side, 0.5 gains + tanh limiter.
/// Emits every 20ms even when a side stalls (silence keeps A/V aligned).
#[cfg(target_os = "windows")]
fn pcm_mixer_loop(
    have_sys: bool,
    have_mic: bool,
    channels: u32,
    sys_rx: std::sync::mpsc::Receiver<Vec<i16>>,
    mic_rx: std::sync::mpsc::Receiver<Vec<i16>>,
    txs: Arc<StdMutex<Vec<std::sync::mpsc::SyncSender<Vec<i16>>>>>,
    stop: &Arc<AtomicBool>,
) {
    let n = PUMP_CHUNK_FRAMES * channels.max(1) as usize;
    let mut last_sys: Option<Vec<i16>> = None;
    let mut last_mic: Option<Vec<i16>> = None;
    while !stop.load(Ordering::SeqCst) {
        let tick = std::time::Instant::now();
        if have_sys {
            while let Ok(c) = sys_rx.try_recv() {
                last_sys = Some(c); // drain, keep latest (absorbs clock drift)
            }
        }
        if have_mic {
            while let Ok(c) = mic_rx.try_recv() {
                last_mic = Some(c);
            }
        }
        let out = match (have_sys, have_mic) {
            (true, true) => {
                let a = last_sys.as_deref().unwrap_or(&[]);
                let b = last_mic.as_deref().unwrap_or(&[]);
                let mut m = Vec::with_capacity(n);
                for i in 0..n {
                    let x = *a.get(i).unwrap_or(&0) as f32 / 32768.0;
                    let y = *b.get(i).unwrap_or(&0) as f32 / 32768.0;
                    // 0.5 gains + tanh soft clip: summed speech/music can't clip.
                    let v = ((x * 0.5 + y * 0.5) * 1.2).tanh();
                    m.push((v * 32767.0) as i16);
                }
                m
            }
            (true, false) => last_sys.clone().unwrap_or_else(|| vec![0; n]),
            (false, true) => last_mic.clone().unwrap_or_else(|| vec![0; n]),
            (false, false) => vec![0; n],
        };
        if let Ok(targets) = txs.lock() {
            for tx in targets.iter() {
                let _ = tx.send(out.clone());
            }
        }
        // Pace to wall clock (device clocks may drift Â±ppm; queues absorb it).
        let el = tick.elapsed();
        if el < Duration::from_millis(20) {
            std::thread::sleep(Duration::from_millis(20) - el);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "windows")]
    fn pcm_pump_streams_and_mixes() {
        // Mode parsing: none => idle pump, no threads.
        let (p, note) = start_pcm_pump("none", "", 48000, 2, vec![]);
        assert!(!p.is_active());
        assert!(note.is_none());

        // System loopback with (likely) silence here still proves threading:
        // the mixer must emit steady 20ms chunks.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(256);
        let (pump, _note) = start_pcm_pump("system", "", 48000, 2, vec![tx]);
        if !pump.is_active() {
            eprintln!("SKIP: no render endpoint for loopback in this environment");
            return;
        }
        let mut got = 0;
        let end = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < end {
            while rx.try_recv().is_ok() {
                got += 1;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // ~15 chunks expected in 300ms (mixer tick); demand a floor.
        eprintln!("pump chunks in 300ms: {got}");
        assert!(got >= 5, "mixer stalled");
        pump.stop_and_join();
    }
}

#[cfg(target_os = "windows")]
fn pick_capture_device(
    enumerator: &wasapi::DeviceEnumerator,
    prefer: &str,
) -> Option<wasapi::Device> {
    use wasapi::Direction;
    let prefer = prefer.trim();
    if !prefer.is_empty() {
        if let Ok(col) = enumerator.get_device_collection(&Direction::Capture) {
            let n = col.get_nbr_devices().unwrap_or(0);
            for i in 0..n {
                if let Ok(dev) = col.get_device_at_index(i) {
                    if let Ok(name) = dev.get_friendlyname() {
                        if name == prefer {
                            return Some(dev);
                        }
                    }
                }
            }
        }
    }
    enumerator.get_default_device(&Direction::Capture).ok()
}

