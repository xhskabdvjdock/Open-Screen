use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrStatus {
    pub available: bool,
    pub engine: String,
    pub tesseract_path: Option<String>,
    pub langs: Vec<String>,
    pub needs_install: bool,
    pub has_eng: bool,
    pub has_ara: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrResult {
    pub text: String,
    pub lang: String,
    pub ms: u128,
}

fn looks_like_real_binary(p: &PathBuf) -> bool {
    // Same LFS-stub guard as the recorder: a real tesseract.exe is hundreds
    // of KB; an unpulled checkout holds a ~130-byte pointer text file.
    std::fs::metadata(p).map(|m| m.len() > 50_000).unwrap_or(false)
}

fn find_tesseract() -> Option<PathBuf> {
    // 1) Shipped inside the installer — always preferred, no download needed.
    if let Some(p) = crate::settings::bundled_file("tesseract/tesseract.exe") {
        if looks_like_real_binary(&p) {
            return Some(p);
        }
    }
    if let Ok(p) = which_tesseract() {
        return Some(p);
    }
    let candidates = [
        r"C:\Program Files\Tesseract-OCR\tesseract.exe",
        r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    let portable = crate::settings::app_dir().join("tesseract").join("tesseract.exe");
    if portable.exists() {
        return Some(portable);
    }
    None
}

fn which_tesseract() -> Result<PathBuf, ()> {
    let name = if cfg!(windows) { "tesseract.exe" } else { "tesseract" };
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(())
}

/// tessdata shipped next to the tesseract binary (may require admin to write to).
fn exe_tessdata_dir(exe: &PathBuf) -> PathBuf {
    exe.parent().unwrap_or(&PathBuf::from(".")).join("tessdata")
}

/// TESSDATA_PREFIX points here; traineddata live in `<app_dir>/tessdata/`.
/// This directory is always user-writable (unlike Program Files).
fn app_data_prefix() -> PathBuf {
    crate::settings::app_dir()
}

fn app_traineddata(lang: &str) -> PathBuf {
    app_data_prefix().join("tessdata").join(format!("{lang}.traineddata"))
}

fn valid_traineddata(p: &PathBuf) -> bool {
    // Real traineddata files are megabytes; reject HTML error pages / stubs.
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) > 100_000
}

fn lang_available(exe: &PathBuf, reported: &[String], lang: &str) -> bool {
    if reported.iter().any(|l| l == lang) {
        return true;
    }
    if valid_traineddata(&exe_tessdata_dir(exe).join(format!("{lang}.traineddata"))) {
        return true;
    }
    if valid_traineddata(&app_traineddata(lang)) {
        return true;
    }
    false
}

fn list_langs(exe: &PathBuf) -> Vec<String> {
    let out = crate::procutil::cmd(exe)
        .arg("--list-langs")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            combined
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.to_lowercase().contains("list of"))
                .collect()
        }
        Err(_) => vec![],
    }
}

async fn download_traineddata(lang: &str, dest: &PathBuf) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let urls = [
        format!("https://github.com/tesseract-ocr/tessdata_fast/raw/main/{lang}.traineddata"),
        format!("https://github.com/tesseract-ocr/tessdata/raw/main/{lang}.traineddata"),
    ];
    let client = reqwest::Client::builder()
        .user_agent("OpenScreen/0.1")
        .build()
        .map_err(|e| e.to_string())?;
    let mut last_err = String::from("unknown error");
    for url in urls {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) if bytes.len() > 100_000 => {
                    std::fs::write(dest, &bytes).map_err(|e| e.to_string())?;
                    return Ok(());
                }
                Ok(bytes) => {
                    last_err = format!("downloaded file too small ({} bytes)", bytes.len());
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            },
            Ok(resp) => {
                last_err = format!("HTTP {}", resp.status());
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
    }
    Err(format!(
        "Could not download {lang} language data ({last_err}). Check your internet connection and retry. / تعذّر تحميل بيانات اللغة. تحقق من الإنترنت وحاول مجددًا."
    ))
}

/// Stage every needed language so tesseract can actually load it.
/// Prefers a free local copy from the install dir; downloads only what's missing.
/// Returns `Some(dir)` to use as TESSDATA_PREFIX, or `None` for tesseract defaults.
async fn resolve_datadir(exe: &PathBuf, langs: &[&str]) -> Result<Option<PathBuf>, String> {
    let exe_dir = exe_tessdata_dir(exe);
    if langs
        .iter()
        .all(|l| valid_traineddata(&exe_dir.join(format!("{l}.traineddata"))))
    {
        return Ok(None);
    }
    for lang in langs {
        let dest = app_traineddata(lang);
        if valid_traineddata(&dest) {
            continue;
        }
        let src = exe_dir.join(format!("{lang}.traineddata"));
        if valid_traineddata(&src) {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&src, &dest).map_err(|e| e.to_string())?;
            continue;
        }
        download_traineddata(lang, &dest).await?;
    }
    Ok(Some(app_data_prefix()))
}

/// Map UI language selection to tesseract codes.
fn resolve_tess_langs(selection: &str, available: &[String]) -> String {
    let want: Vec<&str> = match selection {
        "en" | "eng" | "english" => vec!["eng"],
        "ar" | "ara" | "arabic" => vec!["ara"],
        _ => vec!["eng", "ara"], // auto = both if present, else eng
    };
    let filtered: Vec<&str> = want
        .into_iter()
        .filter(|w| available.is_empty() || available.iter().any(|a| a == w))
        .collect();
    if filtered.is_empty() {
        "eng".to_string()
    } else {
        filtered.join("+")
    }
}

pub async fn ocr_status() -> OcrStatus {
    match find_tesseract() {
        Some(p) => {
            let langs = list_langs(&p);
            OcrStatus {
                has_eng: lang_available(&p, &langs, "eng"),
                has_ara: lang_available(&p, &langs, "ara"),
                available: true,
                engine: "tesseract (local)".to_string(),
                tesseract_path: Some(p.to_string_lossy().to_string()),
                langs,
                needs_install: false,
            }
        }
        None => OcrStatus {
            available: false,
            engine: "none".to_string(),
            tesseract_path: None,
            langs: vec![],
            needs_install: true,
            has_eng: false,
            has_ara: false,
        },
    }
}

/// Pre-warm language data (local copy first, download if needed).
/// `lang`: "ara" / "eng" / "auto" (both).
pub async fn ensure_lang(lang: &str) -> Result<String, String> {
    let exe = find_tesseract().ok_or("OCR_ENGINE_MISSING")?;
    let key = lang.to_lowercase();
    let codes: Vec<&str> = match key.as_str() {
        "ar" | "ara" | "arabic" => vec!["ara"],
        "en" | "eng" | "english" => vec!["eng"],
        _ => vec!["eng", "ara"],
    };
    resolve_datadir(&exe, &codes).await?;
    Ok(format!("{} ready — works offline now.", codes.join("+")))
}

fn run_tesseract(
    exe: &PathBuf,
    img_path: &str,
    langs: &str,
    datadir: &Option<PathBuf>,
    psm: &str,
) -> Result<String, String> {
    let mut cmd = crate::procutil::cmd(exe);
    if let Some(dir) = datadir {
        cmd.env("TESSDATA_PREFIX", dir);
    }
    let out = cmd
        .arg(img_path)
        .arg("stdout")
        .arg("-l")
        .arg(langs)
        .arg("--oem")
        .arg("1")
        .arg("--psm")
        .arg(psm)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        if err.to_lowercase().contains("traineddata") || err.to_lowercase().contains("load language") {
            return Err("LANGDATA".to_string());
        }
        return Err(format!(
            "OCR failed. Try a clearer area or another language. ({})",
            err.lines().next().unwrap_or("tesseract error")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
pub async fn ocr_image_b64(b64: &str, lang_selection: &str) -> Result<OcrResult, String> {
    let exe = find_tesseract().ok_or("OCR_ENGINE_MISSING")?;
    let bytes = B64.decode(b64.trim()).map_err(|e| e.to_string())?;
    // Validate it's an image early for a friendly error
    image::load_from_memory(&bytes).map_err(|_| "Invalid image for OCR")?;

    let available = list_langs(&exe);
    let langs = resolve_tess_langs(&lang_selection.to_lowercase(), &available);
    let parts: Vec<&str> = langs.split('+').collect();

    // Stage language data (local copy first, download if needed).
    // Real errors propagate with a clear message instead of silent failure.
    let datadir = resolve_datadir(&exe, &parts).await?;

    // Write temp image (deleted afterwards per privacy req)
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("openscreen-ocr-{}.png", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, &bytes).await.map_err(|e| e.to_string())?;

    let started = std::time::Instant::now();
    let tmp_str = tmp.to_string_lossy().to_string();
    let langs_clone = langs.clone();
    let datadir_clone = datadir.clone();
    let text = tokio::task::spawn_blocking(move || {
        // PSM 6 (uniform block) first; if it finds nothing, retry with
        // PSM 3 (fully automatic) — much better for Arabic passages.
        let t = run_tesseract(&exe, &tmp_str, &langs_clone, &datadir_clone, "6")?;
        if t.trim().is_empty() {
            run_tesseract(&exe, &tmp_str, &langs_clone, &datadir_clone, "3")
        } else {
            Ok(t)
        }
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e: String| {
        if e == "LANGDATA" {
            "Arabic/English data missing and download failed. Connect to the internet and press Download, then retry. / بيانات اللغة غير موجودة وتعذّر تحميلها. اتصل بالإنترنت وحمّلها ثم أعد المحاولة.".to_string()
        } else {
            e
        }
    })?;

    let _ = tokio::fs::remove_file(&tmp).await;
    let ms = started.elapsed().as_millis();

    Ok(OcrResult {
        text,
        lang: langs,
        ms,
    })
}

pub async fn install_via_winget() -> Result<String, String> {
    // UB-Mannheim Tesseract (includes eng; ara tessdata staged on first use)
    let mut cmd = crate::procutil::tokio_cmd("winget");
    cmd.args([
        "install",
        "-e",
        "--id",
        "UB-Mannheim.TesseractOCR",
        "--silent",
        "--accept-package-agreements",
        "--accept-source-agreements",
    ]);
    let out = cmd.output().await.map_err(|e| format!("Failed to launch winget: {e}"))?;
    if out.status.success() {
        Ok("Tesseract installed. If OCR still reports missing, restart Open Screen.".to_string())
    } else {
        Err(format!(
            "winget install failed: {}",
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("unknown error")
        ))
    }
}
