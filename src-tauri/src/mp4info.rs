//! Minimal MP4 box parser — ffprobe-free file verification.
//!
//! Reads structure only (no media decode):
//! - video track: width, height, codec fourcc, sample count, duration
//! - audio track: codec fourcc, sample count, duration
//! - movie duration
//! Used to verify native recordings honestly (resolution/FPS/codec/duration)
//! and to drive the segment remuxer (`mp4mux.rs`).

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mp4TrackInfo {
    pub kind: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub samples: u32,
    pub duration_sec: f64,
    pub timescale: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mp4Info {
    pub duration_sec: f64,
    pub video: Option<Mp4TrackInfo>,
    pub audio: Option<Mp4TrackInfo>,
    pub valid: bool,
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    fn u32(&mut self) -> Option<u32> {
        if self.rest() < 4 {
            return None;
        }
        let v = u32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().ok()?);
        self.pos += 4;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        if self.rest() < 8 {
            return None;
        }
        let v = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().ok()?);
        self.pos += 8;
        Some(v)
    }
    fn tag(&mut self) -> Option<[u8; 4]> {
        if self.rest() < 4 {
            return None;
        }
        let t: [u8; 4] = self.data[self.pos..self.pos + 4].try_into().ok()?;
        self.pos += 4;
        Some(t)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.rest() < n {
            return None;
        }
        self.pos += n;
        Some(())
    }
    fn slice(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest() < n {
            return None;
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }
}

struct BoxRef<'a> {
    tag: [u8; 4],
    body: &'a [u8],
}

fn tag_str(t: &[u8; 4]) -> String {
    String::from_utf8_lossy(t).to_string()
}

fn children<'a>(body: &'a [u8]) -> Vec<BoxRef<'a>> {
    let mut out = vec![];
    let mut c = Cursor { data: body, pos: 0 };
    while c.rest() >= 8 {
        let start = c.pos;
        let mut size = c.u32().unwrap_or(0) as u64;
        let tag = match c.tag() {
            Some(t) => t,
            None => break,
        };
        let mut header = 8u64;
        if size == 1 {
            size = match c.u64() {
                Some(s) => s,
                None => break,
            };
            header = 16;
        } else if size == 0 {
            size = c.rest() as u64 + header;
        }
        if size < header || (start as u64) + size > body.len() as u64 {
            break;
        }
        let body_len = (size - header) as usize;
        let _ = start;
        match c.slice(body_len) {
            Some(b) => out.push(BoxRef { tag, body: b }),
            None => break,
        }
    }
    out
}

fn full_header(c: &mut Cursor) -> Option<(u8, u32)> {
    let v = c.u32()?;
    Some(((v >> 24) as u8, v & 0x00FF_FFFF))
}

#[derive(Default)]
struct TrackCtx {
    kind: String,
    codec: String,
    width: u32,
    height: u32,
    samples: u32,
    timescale: u32,
    duration: u64,
}

fn parse_trak(body: &[u8]) -> Option<TrackCtx> {
    let mut t = TrackCtx::default();
    for b in children(body) {
        match &b.tag {
            b"tkhd" => {
                let mut c = Cursor { data: b.body, pos: 0 };
                let (ver, _) = full_header(&mut c)?;
                if ver == 1 {
                    c.skip(8 + 8 + 4 + 8)?;
                } else {
                    c.skip(4 + 4 + 4 + 8)?;
                }
                c.skip(8 + 2 + 2 + 2 + 2)?;
                c.skip(36)?;
                let w = c.u32()?;
                let h = c.u32()?;
                t.width = w >> 16;
                t.height = h >> 16;
            }
            b"mdia" => {
                for m in children(b.body) {
                    match &m.tag {
                        b"mdhd" => {
                            let mut c = Cursor { data: m.body, pos: 0 };
                            let (ver, _) = full_header(&mut c)?;
                            if ver == 1 {
                                c.skip(8 + 8)?;
                                t.timescale = c.u32()?;
                                t.duration = c.u64()?;
                            } else {
                                c.skip(4 + 4)?;
                                t.timescale = c.u32()?;
                                t.duration = c.u32()? as u64;
                            }
                        }
                        b"hdlr" => {
                            let mut c = Cursor { data: m.body, pos: 0 };
                            full_header(&mut c)?;
                            c.skip(4)?;
                            let h = c.tag()?;
                            t.kind = match &h {
                                b"vide" => "video".to_string(),
                                b"soun" => "audio".to_string(),
                                _ => tag_str(&h),
                            };
                        }
                        b"minf" => {
                            for f in children(m.body) {
                                if &f.tag == b"stbl" {
                                    for s in children(f.body) {
                                        match &s.tag {
                                            b"stsd" => {
                                                let mut c = Cursor { data: s.body, pos: 0 };
                                                full_header(&mut c)?;
                                                c.skip(4)?;
                                                if c.rest() >= 8 {
                                                    let _sz = c.u32()?;
                                                    if let Some(cd) = c.tag() {
                                                        t.codec = tag_str(&cd);
                                                    }
                                                }
                                            }
                                            b"stsz" => {
                                                let mut c = Cursor { data: s.body, pos: 0 };
                                                full_header(&mut c)?;
                                                let sample_size = c.u32()?;
                                                let count = c.u32()?;
                                                t.samples = count;
                                                let _ = sample_size;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if t.kind.is_empty() {
        return None;
    }
    Some(t)
}

pub fn read_info(path: &Path) -> Result<Mp4Info, String> {
    let data = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if data.len() < 32 {
        return Err("File too small to be MP4.".to_string());
    }
    let mut info = Mp4Info::default();
    for b in children(&data) {
        if &b.tag == b"moov" {
            for m in children(b.body) {
                match &m.tag {
                    b"mvhd" => {
                        let mut c = Cursor { data: m.body, pos: 0 };
                        let (ver, _) = full_header(&mut c).unwrap_or((0, 0));
                        if ver == 1 {
                            c.skip(8 + 8).ok_or("Truncated mvhd.")?;
                            let ts = c.u32().unwrap_or(0);
                            let du = c.u64().unwrap_or(0);
                            if ts > 0 {
                                info.duration_sec = du as f64 / ts as f64;
                            }
                        } else {
                            c.skip(4 + 4).ok_or("Truncated mvhd.")?;
                            let ts = c.u32().unwrap_or(0);
                            let du = c.u32().unwrap_or(0);
                            if ts > 0 {
                                info.duration_sec = du as f64 / ts as f64;
                            }
                        }
                    }
                    b"trak" => {
                        if let Some(t) = parse_trak(m.body) {
                            let ti = Mp4TrackInfo {
                                duration_sec: if t.timescale > 0 {
                                    t.duration as f64 / t.timescale as f64
                                } else {
                                    0.0
                                },
                                timescale: t.timescale,
                                kind: t.kind.clone(),
                                codec: t.codec,
                                width: t.width,
                                height: t.height,
                                samples: t.samples,
                            };
                            match ti.kind.as_str() {
                                "video" => info.video = Some(ti),
                                "audio" => info.audio = Some(ti),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    info.valid = info.video.is_some();
    if !info.valid {
        return Err("No video track found — not a playable MP4.".to_string());
    }
    Ok(info)
}

/// Honest FPS from the container: video samples / video duration.
pub fn container_fps(info: &Mp4Info) -> f64 {
    match &info.video {
        Some(v) if v.duration_sec > 0.2 => v.samples as f64 / v.duration_sec,
        _ => 0.0,
    }
}
