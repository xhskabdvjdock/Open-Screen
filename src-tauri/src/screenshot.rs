use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Local;
use image::{DynamicImage, ImageBuffer, Rgba};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use xcap::Monitor;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureResult {
    pub base64: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
}

fn dyn_to_png_base64(img: &DynamicImage) -> Result<(String, u32, u32), String> {
    let (w, h) = (img.width(), img.height());
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    drop(cursor);
    Ok((B64.encode(&buf), w, h))
}

fn xcap_image_to_dynamic(img: xcap::image::RgbaImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let raw = img.into_raw();
    let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(w, h, raw).expect("xcap buffer size");
    DynamicImage::ImageRgba8(buf)
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    Ok(monitors
        .iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            index: i,
            name: m.name().unwrap_or_else(|_| format!("Screen {}", i + 1)),
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            scale: 1.0,
        })
        .collect())
}

/// Capture an arbitrary virtual-screen rectangle, stitching across monitors.
pub fn capture_region(x: i32, y: i32, w: u32, h: u32) -> Result<CaptureResult, String> {
    if w == 0 || h == 0 {
        return Err("Empty region".to_string());
    }
    if w > 8000 || h > 8000 {
        return Err("Region too large".to_string());
    }
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    if monitors.is_empty() {
        return Err("No monitors found".to_string());
    }
    let mut canvas: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    let mut painted = false;

    for m in monitors.iter() {
        let mx = m.x().unwrap_or(0);
        let my = m.y().unwrap_or(0);
        let mw = m.width().unwrap_or(0) as i32;
        let mh = m.height().unwrap_or(0) as i32;
        // intersection of requested region with this monitor
        let ix0 = x.max(mx);
        let iy0 = y.max(my);
        let ix1 = (x + w as i32).min(mx + mw);
        let iy1 = (y + h as i32).min(my + mh);
        if ix1 <= ix0 || iy1 <= iy0 {
            continue;
        }
        let shot = m.capture_image().map_err(|e| e.to_string())?;
        let dyn_img = xcap_image_to_dynamic(shot);
        let (sw, sh) = (dyn_img.width(), dyn_img.height());
        // source coords inside monitor screenshot
        let sx = (ix0 - mx).max(0) as u32;
        let sy = (iy0 - my).max(0) as u32;
        let sw_ = ((ix1 - ix0) as u32).min(sw.saturating_sub(sx));
        let sh_ = ((iy1 - iy0) as u32).min(sh.saturating_sub(sy));
        if sw_ == 0 || sh_ == 0 {
            continue;
        }
        let cropped = dyn_img.crop_imm(sx, sy, sw_, sh_).to_rgba8();
        let dx = (ix0 - x) as u32;
        let dy = (iy0 - y) as u32;
        for (cx, cy, px) in cropped.enumerate_pixels() {
            canvas.put_pixel(dx + cx, dy + cy, *px);
        }
        painted = true;
    }

    if !painted {
        return Err("Selected area is outside all monitors".to_string());
    }
    let dyn_img = DynamicImage::ImageRgba8(canvas);
    let (base64, width, height) = dyn_to_png_base64(&dyn_img)?;
    Ok(CaptureResult {
        base64,
        mime: "image/png".to_string(),
        width,
        height,
    })
}

pub fn capture_monitor(index: usize) -> Result<CaptureResult, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let m = monitors.get(index).ok_or("Monitor not found")?;
    let shot = m.capture_image().map_err(|e| e.to_string())?;
    let dyn_img = xcap_image_to_dynamic(shot);
    let (base64, width, height) = dyn_to_png_base64(&dyn_img)?;
    Ok(CaptureResult {
        base64,
        mime: "image/png".to_string(),
        width,
        height,
    })
}

pub fn capture_all() -> Result<CaptureResult, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    if monitors.is_empty() {
        return Err("No monitors found".to_string());
    }
    let min_x = monitors.iter().map(|m| m.x().unwrap_or(0)).min().unwrap_or(0);
    let min_y = monitors.iter().map(|m| m.y().unwrap_or(0)).min().unwrap_or(0);
    let max_x = monitors
        .iter()
        .map(|m| m.x().unwrap_or(0) + m.width().unwrap_or(0) as i32)
        .max()
        .unwrap_or(0);
    let max_y = monitors
        .iter()
        .map(|m| m.y().unwrap_or(0) + m.height().unwrap_or(0) as i32)
        .max()
        .unwrap_or(0);
    let w = (max_x - min_x).max(1) as u32;
    let h = (max_y - min_y).max(1) as u32;
    capture_region(min_x, min_y, w, h)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveWindowInfo {
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub fn active_window_info() -> Result<ActiveWindowInfo, String> {
    let pos = active_win_pos_rs::get_active_window().map_err(|e| format!("{e:?}"))?;
    Ok(ActiveWindowInfo {
        title: pos.title,
        x: pos.position.x as i32,
        y: pos.position.y as i32,
        width: pos.position.width.max(1.0) as u32,
        height: pos.position.height.max(1.0) as u32,
    })
}

pub fn capture_active_window() -> Result<CaptureResult, String> {
    let info = active_window_info()?;
    capture_region(info.x, info.y, info.width, info.height)
}

pub fn decode_base64_png(b64: &str) -> Result<DynamicImage, String> {
    let bytes = B64.decode(b64.trim()).map_err(|e| e.to_string())?;
    image::load_from_memory(&bytes).map_err(|e| e.to_string())
}

pub fn encode_image_base64(img: &DynamicImage, format: &str, quality: u8) -> Result<(String, String), String> {
    let fmt = format.to_lowercase();
    let mut buf = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut buf);
        match fmt.as_str() {
            "jpg" | "jpeg" => {
                let rgb = img.to_rgb8();
                let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
                enc.encode_image(&rgb).map_err(|e| e.to_string())?;
            }
            _ => {
                img.write_to(&mut cursor, image::ImageFormat::Png)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    let mime = if fmt == "jpg" || fmt == "jpeg" {
        "image/jpeg"
    } else {
        "image/png"
    };
    Ok((B64.encode(&buf), mime.to_string()))
}

pub fn default_pictures_dir() -> PathBuf {
    dirs::picture_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Open Screen")
}

pub fn generate_filename(format: &str, pattern: &str) -> String {
    let now = Local::now();
    let date = now.format("%Y-%m-%d").to_string();
    let time = now.format("%H-%M-%S").to_string();
    let base = pattern
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{format}", format);
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let with_prefix = if clean.starts_with("OpenScreen") || clean.starts_with("Open_Screen") {
        clean
    } else {
        clean
    };
    format!("{with_prefix}.{format}")
}
