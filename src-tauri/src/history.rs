use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub id: String,
    pub created_at: String,
    pub width: u32,
    pub height: u32,
    pub file_name: String,
    pub image_path: String,
    pub thumb_base64: String,
}

fn history_dir() -> PathBuf {
    crate::settings::app_dir().join("history")
}

fn index_path() -> PathBuf {
    history_dir().join("index.json")
}

fn load_index() -> Vec<HistoryItem> {
    std::fs::read(index_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<HistoryItem>>(&b).ok())
        .unwrap_or_default()
}

fn save_index(items: &[HistoryItem]) {
    let _ = std::fs::create_dir_all(history_dir());
    if let Ok(bytes) = serde_json::to_vec_pretty(items) {
        let _ = std::fs::write(index_path(), bytes);
    }
}

fn make_thumbnail(png_bytes: &[u8]) -> String {
    // Small JPEG-ish PNG thumbnail (max 320px) for list rendering; keeps RAM low.
    let img = image::load_from_memory(png_bytes);
    match img {
        Ok(img) => {
            let thumb = img.thumbnail(320, 200);
            let mut buf = Vec::new();
            let mut cur = std::io::Cursor::new(&mut buf);
            if thumb.write_to(&mut cur, image::ImageFormat::Png).is_ok() {
                return B64.encode(&buf);
            }
            String::new()
        }
        Err(_) => String::new(),
    }
}

pub fn history_list() -> Vec<HistoryItem> {
    let mut items = load_index();
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    items
}

pub fn history_add(base64_png: &str, width: u32, height: u32, limit: usize) -> Result<HistoryItem, String> {
    let bytes = B64.decode(base64_png.trim()).map_err(|e| e.to_string())?;
    let dir = history_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Local::now().to_rfc3339();
    let file_name = format!("{id}.png");
    std::fs::write(dir.join(&file_name), &bytes).map_err(|e| e.to_string())?;
    let thumb_base64 = make_thumbnail(&bytes);
    let item = HistoryItem {
        id: id.clone(),
        created_at,
        width,
        height,
        file_name: file_name.clone(),
        image_path: dir.join(&file_name).to_string_lossy().to_string(),
        thumb_base64,
    };
    let mut items = load_index();
    items.push(item.clone());
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    // Enforce limit (delete oldest files)
    let lim = limit.clamp(1, 100);
    while items.len() > lim {
        if let Some(old) = items.pop() {
            let _ = std::fs::remove_file(history_dir().join(&old.file_name));
        }
    }
    save_index(&items);
    Ok(item)
}

pub fn history_get(id: &str) -> Result<String, String> {
    let items = load_index();
    let item = items.iter().find(|i| i.id == id).ok_or("History item not found")?;
    let bytes = std::fs::read(history_dir().join(&item.file_name)).map_err(|e| e.to_string())?;
    Ok(B64.encode(&bytes))
}

pub fn history_delete(id: &str) -> Result<(), String> {
    let mut items = load_index();
    if let Some(pos) = items.iter().position(|i| i.id == id) {
        let item = items.remove(pos);
        let _ = std::fs::remove_file(history_dir().join(&item.file_name));
        save_index(&items);
    }
    Ok(())
}

pub fn history_clear() -> Result<(), String> {
    let items = load_index();
    for i in items {
        let _ = std::fs::remove_file(history_dir().join(&i.file_name));
    }
    save_index(&[]);
    Ok(())
}
