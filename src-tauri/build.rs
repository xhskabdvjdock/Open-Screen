fn main() {
    // Fail-fast guard: never ship an installer whose bundled engines are
    // Git LFS pointer files (unpulled checkout). Those ~130-byte text files
    // crash at runtime with Windows os error 216. Warn loudly here; the app
    // itself re-validates sizes at startup and falls back to the downloader.
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // Recording is native (no bundled binaries). Only the OCR engine is
    // still shipped; warn if it is an unpulled Git LFS pointer.
    for (rel, min_bytes) in [("resources/tesseract/tesseract.exe", 50_000u64)] {
        let p = manifest.join(rel);
        if let Ok(m) = std::fs::metadata(&p) {
            if m.len() < min_bytes {
                println!(
                    "cargo:warning={rel} is {} bytes (expected >= {min_bytes}) — looks like an unpulled Git LFS pointer. OCR will fall back to winget. Run scripts/fetch-engines.ps1 or `git lfs pull` before release builds.",
                    m.len()
                );
            }
        }
    }
    tauri_build::build()
}
