//! Pure-Rust MP4 segment remuxer (no FFmpeg concat).
//!
//! Recording pause/resume and Instant Replay both produce independently-valid
//! MP4 segments (each starts with a keyframe â€” fresh encoder per segment).
//! This module concatenates them with **stream copy**: samples are re-based
//! onto one continuous timeline per track, codecs untouched, no re-encode.
//! Single-segment inputs are moved through untouched (byte-identical fast path).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct RemuxInfo {
    pub video_samples: u64,
    pub audio_samples: u64,
    pub duration_sec: f64,
}

#[derive(Clone)]
struct TrackPlan {
    timescale: u32,
}

pub fn remux_segments(files: &[PathBuf], out: &Path) -> Result<RemuxInfo, String> {
    remux_inner(files, out, None)
}

/// Merge with a head trim: keep only the last `keep_secs` seconds of the
/// video timeline (replay save). Video starts at the nearest preceding
/// keyframe (no broken GOP); audio starts at the first sample covering the
/// video start. Overshoot beyond `keep_secs` is bounded by one keyframe
/// interval and reported via the returned duration.
pub fn remux_segments_keep_last(
    files: &[PathBuf],
    out: &Path,
    keep_secs: f64,
) -> Result<RemuxInfo, String> {
    remux_inner(files, out, Some(keep_secs.max(1.0)))
}

fn remux_inner(files: &[PathBuf], out: &Path, keep: Option<f64>) -> Result<RemuxInfo, String> {
    if files.is_empty() {
        return Err("Nothing to merge.".to_string());
    }
    if files.len() == 1 {
        // Fast path: single part â€” move, don't even rewrite.
        if std::fs::rename(&files[0], out).is_err() {
            std::fs::copy(&files[0], out).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&files[0]);
        }
        let info = crate::mp4info::read_info(out).map_err(|e| e.to_string())?;
        return Ok(RemuxInfo {
            video_samples: info.video.as_ref().map(|v| v.samples as u64).unwrap_or(0),
            audio_samples: info.audio.as_ref().map(|a| a.samples as u64).unwrap_or(0),
            duration_sec: info.duration_sec,
        });
    }

    // 1) Open all segments, resolve video+audio tracks and codec configs.
    struct Open {
        reader: mp4::Mp4Reader<BufReader<File>>,
        video: Option<(u32, TrackPlan)>,
        audio: Option<(u32, TrackPlan)>,
        avc: Option<mp4::AvcConfig>,
        aac: Option<mp4::AacConfig>,
    }
    let mut segs: Vec<Open> = vec![];
    for f in files {
        let file = File::open(f).map_err(|e| format!("Cannot open {}: {e}", f.display()))?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        let reader = mp4::Mp4Reader::read_header(BufReader::new(file), size)
            .map_err(|e| format!("Bad segment {}: {e}", f.display()))?;
        let mut video = None;
        let mut audio = None;
        let mut avc = None;
        let mut aac = None;
        for (id, track) in reader.tracks().iter() {
            let ttype = track.track_type().map_err(|e| e.to_string())?;
            match ttype {
                mp4::TrackType::Video => {
                    if video.is_none() {
                        let sps = track.sequence_parameter_set().map_err(|e| e.to_string())?.to_vec();
                        let pps = track.picture_parameter_set().map_err(|e| e.to_string())?.to_vec();
                        avc = Some(mp4::AvcConfig {
                            width: track.width(),
                            height: track.height(),
                            seq_param_set: sps,
                            pic_param_set: pps,
                        });
                        video = Some((
                            *id,
                            TrackPlan { timescale: track.timescale() },
                        ));
                    }
                }
                mp4::TrackType::Audio => {
                    if audio.is_none() {
                        aac = Some(mp4::AacConfig {
                            bitrate: track.bitrate(),
                            profile: track.audio_profile().map_err(|e| e.to_string())?,
                            freq_index: track.sample_freq_index().map_err(|e| e.to_string())?,
                            chan_conf: track.channel_config().map_err(|e| e.to_string())?,
                        });
                        audio = Some((
                            *id,
                            TrackPlan { timescale: track.timescale() },
                        ));
                    }
                }
                _ => {}
            }
        }
        if video.is_none() {
            return Err(format!("Segment has no video track: {}", f.display()));
        }
        segs.push(Open { reader, video, audio, avc, aac });
    }

    // All segments must share geometry/timescales (same pipeline produced them).
    let first = &segs[0];
    let (v_id, vplan) = first.video.as_ref().unwrap().clone();
    let vcfg = first.avc.clone().unwrap();
    let vts = vplan.timescale;
    let has_audio = segs.iter().any(|s| s.audio.is_some());
    let (a_id, aplan, acfg, ats) = if has_audio {
        let s = segs
            .iter()
            .find(|s| s.audio.is_some() && s.aac.is_some())
            .ok_or("Audio track advertised but unreadable.")?;
        let (id, plan) = s.audio.as_ref().unwrap().clone();
        (Some(id), Some(plan.clone()), s.aac.clone(), plan.timescale)
    } else {
        (None, None, None, 0)
    };
    let _ = (a_id, v_id);
    for s in &segs[1..] {
        let ( _, vp) = s.video.as_ref().unwrap();
        if vp.timescale != vts {
            return Err("Segment timescale mismatch â€” cannot stream-copy merge.".to_string());
        }
        if let Some(c) = &s.avc {
            if c.seq_param_set != vcfg.seq_param_set {
                return Err("Segment codec config changed mid-recording â€” cannot merge.".to_string());
            }
        }
    }

    // 2) Stream the merge: segment by segment, track by track, rebased onto
    // one timeline per track. Only one segment is mapped at a time (no giant
    // RAM spike on long replays).
    let out_file = File::create(out).map_err(|e| format!("Cannot create {}: {e}", out.display()))?;
    let mp4cfg = mp4::Mp4Config {
        major_brand: "isom".parse().map_err(|e| format!("{e}"))?,
        minor_version: 512,
        compatible_brands: vec![
            "isom".parse().map_err(|e| format!("{e}"))?,
            "iso2".parse().map_err(|e| format!("{e}"))?,
            "avc1".parse().map_err(|e| format!("{e}"))?,
            "mp41".parse().map_err(|e| format!("{e}"))?,
        ],
        timescale: 1000,
    };
    let mut writer = mp4::Mp4Writer::write_start(out_file, &mp4cfg).map_err(|e| e.to_string())?;
    let vtrack = mp4::TrackConfig {
        track_type: mp4::TrackType::Video,
        timescale: vts,
        language: "und".to_string(),
        media_conf: mp4::MediaConfig::AvcConfig(vcfg),
    };
    writer.add_track(&vtrack).map_err(|e| e.to_string())?;
    if let (Some(ac), Some(_)) = (acfg, &aplan) {
        let atrack = mp4::TrackConfig {
            track_type: mp4::TrackType::Audio,
            timescale: ats,
            language: "und".to_string(),
            media_conf: mp4::MediaConfig::AacConfig(ac),
        };
        writer.add_track(&atrack).map_err(|e| e.to_string())?;
    }
    const VOUT: u32 = 1;
    const AOUT: u32 = 2;

    // Trim plan: global video start ordinal + per-track rebase offsets.
    // (start, duration, is_sync) headers are scanned first so trimming never
    // needs sample bytes twice; the merge pass below streams bytes once.
    struct Head {
        start: u64,
        dur: u32,
        sync: bool,
    }
    let mut v_heads: Vec<Vec<Head>> = vec![];
    let mut a_heads: Vec<Vec<Head>> = vec![];
    let mut v_total: u64 = 0;
    for s in segs.iter_mut() {
        let (src_vid, _) = s.video.clone().unwrap();
        let n_v = s.reader.sample_count(src_vid).map_err(|e| e.to_string())?;
        let mut hv = Vec::with_capacity(n_v as usize);
        for sid in 1..=n_v {
            match s.reader.read_sample(src_vid, sid).map_err(|e| e.to_string())? {
                Some(sm) => {
                    hv.push(Head { start: sm.start_time, dur: sm.duration, sync: sm.is_sync });
                    v_total = v_total.max(sm.start_time + sm.duration as u64);
                }
                None => hv.push(Head { start: 0, dur: 0, sync: false }),
            }
        }
        v_heads.push(hv);
        let mut ha = vec![];
        if let Some((src_aid, _)) = s.audio.clone() {
            let n_a = s.reader.sample_count(src_aid).map_err(|e| e.to_string())?;
            for sid in 1..=n_a {
                match s.reader.read_sample(src_aid, sid).map_err(|e| e.to_string())? {
                    Some(sm) => ha.push(Head { start: sm.start_time, dur: sm.duration, sync: sm.is_sync }),
                    None => ha.push(Head { start: 0, dur: 0, sync: false }),
                }
            }
        }
        a_heads.push(ha);
    }
    // Global segment bases first (trim math needs the true timeline end).
    let seg_end = |hv: &Vec<Head>| hv.last().map(|h| h.start + h.dur as u64).unwrap_or(0);
    let mut gv_pre = vec![0u64; v_heads.len() + 1];
    for (i, hv) in v_heads.iter().enumerate() {
        gv_pre[i + 1] = gv_pre[i] + seg_end(hv);
    }
    v_total = gv_pre[v_heads.len()];
    // Global video start ordinal: last sync sample at/before the cutoff.
    let mut v_skip_ord: u64 = 0;
    let mut v_start_ts: u64 = 0;
    if let Some(keep) = keep {
        if vts > 0 {
            let cutoff = v_total.saturating_sub((keep * vts as f64) as u64);
            if cutoff > 0 {
                let mut ord = 0u64;
                let mut best_ord = 0u64;
                let mut best_ts = 0u64;
                // Per-segment timelines restart at 0 â€” track segment base.
                let mut base = 0u64;
                for (si, hv) in v_heads.iter().enumerate() {
                    // Segment end in that segment's local timeline:
                    let seg_end = hv.last().map(|h| h.start + h.dur as u64).unwrap_or(0);
                    for h in hv {
                        let g = base + h.start;
                        if h.sync && g <= cutoff {
                            best_ord = ord;
                            best_ts = g;
                        }
                        ord += 1;
                    }
                    base += seg_end;
                    let _ = si;
                }
                v_skip_ord = best_ord;
                v_start_ts = best_ts;
            }
        }
    }
    // Audio starts at the first sample covering the video start (exact,
    // AAC frames are ~21ms â€” no keyframe constraint).
    let v_start_sec = if vts > 0 { v_start_ts as f64 / vts as f64 } else { 0.0 };
    let mut a_skip_ord: u64 = 0;
    let mut a_start_ts: u64 = 0;
    if v_skip_ord > 0 && ats > 0 {
        let mut ord = 0u64;
        let mut base = 0u64;
        'outer: for ha in a_heads.iter() {
            let seg_end = ha.last().map(|h| h.start + h.dur as u64).unwrap_or(0);
            for h in ha {
                let g_sec = (base + h.start) as f64 / ats as f64;
                let g_end = g_sec + h.dur as f64 / ats as f64;
                if g_end > v_start_sec {
                    a_skip_ord = ord;
                    a_start_ts = base + h.start;
                    break 'outer;
                }
                ord += 1;
            }
            base += seg_end;
        }
    }

    // Global segment bases (prefix sums of segment-local ends): sample (si,
    // local) lives at global G[si] + local. Output = global - trim origin,
    // so the file always starts at 00:00 (#43).
    let gv = gv_pre;
    let mut ga = vec![0u64; a_heads.len() + 1];
    for (i, ha) in a_heads.iter().enumerate() {
        ga[i + 1] = ga[i] + seg_end(ha);
    }

    let mut info = RemuxInfo::default();
    let mut v_ord: u64 = 0;
    let mut a_ord: u64 = 0;
    for (si, s) in segs.iter_mut().enumerate() {
        let (src_vid, _) = s.video.clone().unwrap();
        let n_v = s.reader.sample_count(src_vid).map_err(|e| e.to_string())?;
        for sid in 1..=n_v {
            let g = v_ord;
            v_ord += 1;
            if g < v_skip_ord {
                continue;
            }
            if let Some(mut sample) = s.reader.read_sample(src_vid, sid).map_err(|e| e.to_string())? {
                sample.start_time = gv[si] + sample.start_time - v_start_ts;
                writer.write_sample(VOUT, &sample).map_err(|e| e.to_string())?;
                info.video_samples += 1;
            }
        }
        if s.audio.clone().is_some() {
            let (src_aid, _) = s.audio.clone().unwrap();
            let n_a = s.reader.sample_count(src_aid).map_err(|e| e.to_string())?;
            for sid in 1..=n_a {
                let g = a_ord;
                a_ord += 1;
                if g < a_skip_ord {
                    continue;
                }
                if let Some(mut sample) = s.reader.read_sample(src_aid, sid).map_err(|e| e.to_string())? {
                    sample.start_time = ga[si] + sample.start_time - a_start_ts;
                    writer.write_sample(AOUT, &sample).map_err(|e| e.to_string())?;
                    info.audio_samples += 1;
                }
            }
        }
    }
    writer.write_end().map_err(|e| e.to_string())?;
    info.duration_sec = if vts > 0 {
        gv[v_heads.len()].saturating_sub(v_start_ts) as f64 / vts as f64
    } else {
        0.0
    };
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "windows")]
    fn remux_two_segments_keeps_one_timeline() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("openscreen-remuxtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = crate::nativerec::NativeRecConfig {
            monitor_idx: 0,
            crop: None,
            out_w: 1920,
            out_h: 1080,
            fps: 60,
            bitrate_bps: 12_000_000,
            codec: crate::nativerec::NativeCodec::H264,
            cursor: true,
            audio_channels: 0,
            sample_rate: 48000,
        };
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(8));
        let p1 = dir.join("seg1.mp4");
        let p2 = dir.join("seg2.mp4");
        crate::nativerec::record_seconds(cfg.clone(), p1.clone(), 3).expect("seg1 failed");
        crate::nativerec::record_seconds(cfg, p2.clone(), 3).expect("seg2 failed");
        let _ = wiggler.join();
        let n1 = crate::mp4info::read_info(&p1).unwrap().video.unwrap().samples as u64;
        let n2 = crate::mp4info::read_info(&p2).unwrap().video.unwrap().samples as u64;
        let out = dir.join("merged.mp4");
        let info = remux_segments(&[p1, p2], &out).expect("remux failed");
        eprintln!("remux: {info:?} (parts {n1}+{n2})");
        assert_eq!(info.video_samples, n1 + n2, "sample loss in merge");
        assert!((3.0..=5.5).contains(&info.duration_sec), "bad merged duration");
        let back = crate::mp4info::read_info(&out).expect("merged unreadable");
        let v = back.video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (1920, 1080));
        assert_eq!(v.samples as u64, n1 + n2);
        let fps = crate::mp4info::container_fps(&back);
        assert!((50.0..=70.0).contains(&fps), "merged fps off: {fps}");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn remux_keep_last_trims_to_window() {
        let _capture_guard = crate::wgc::CAPTURE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("openscreen-trimtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = crate::nativerec::NativeRecConfig {
            monitor_idx: 0,
            crop: None,
            out_w: 1280,
            out_h: 720,
            fps: 30,
            bitrate_bps: 6_000_000,
            codec: crate::nativerec::NativeCodec::H264,
            cursor: true,
            audio_channels: 0,
            sample_rate: 48000,
        };
        let wiggler = std::thread::spawn(|| crate::wgc::wiggle_cursor(10));
        let mut parts = vec![];
        for i in 0..3 {
            let p = dir.join(format!("t{i}.mp4"));
            crate::nativerec::record_seconds(cfg.clone(), p.clone(), 3).expect("segment failed");
            parts.push(p);
        }
        let _ = wiggler.join();
        let full = dir.join("full.mp4");
        let fi = remux_segments(&parts, &full).expect("full remux failed");
        eprintln!("full: {fi:?}");
        // Keep last 3s of ~6-9s of footage.
        let out = dir.join("trim.mp4");
        let ti = remux_segments_keep_last(&parts, &out, 3.0).expect("trim remux failed");
        eprintln!("trimmed: {ti:?}");
        assert!(ti.duration_sec <= 4.5, "trim overshoot too big: {}", ti.duration_sec);
        assert!(ti.duration_sec >= 2.0, "trim cut too much: {}", ti.duration_sec);
        assert!(ti.video_samples < fi.video_samples, "nothing trimmed");
        assert!(ti.video_samples > 30, "trimmed to almost nothing");
        let back = crate::mp4info::read_info(&out).expect("trimmed unreadable");
        let v = back.video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (1280, 720));
    }
}
