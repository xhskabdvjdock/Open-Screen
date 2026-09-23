# Open Screen — Screenshot Tool + Lightweight Image Editor + Local OCR

**Capture fast. Edit when needed. OCR when needed. Save only when needed.**

Lightweight, local-first Windows desktop app built with **Rust + Tauri** (no Electron).

## Features
- System tray (Take Screenshot / OCR / History / Settings / Quit), no always-open window
- Global hotkeys: `Ctrl+Shift+S` screenshot, `Ctrl+Shift+O` OCR (both remappable)
- Capture mode pill (top-center): Screenshot / Text / **Record** — `Tab` cycles, one shortcut for all
- Screen recording (fully native: Windows Graphics Capture + Media Foundation + WASAPI, no downloads): area/fullscreen/monitor/all-screens/window, pause/resume (lossless stream-copy merge), MP4 H.264/HEVC, hardware encoding first (Media Foundation HW MFT → inbox software fallback with notice), mic/system/mixed audio, cursor toggle
- Instant Replay: rolling temp buffer (10–300s + custom), `Ctrl+Shift+I` saves last N seconds as `OpenScreen_Replay_*.mp4`, buffer keeps rolling after save
- Overlay selection: Region (+ live `W × H`), Fullscreen (`F`), Active window (`W`), All screens (`A`), multi-monitor aware, Esc cancels
- After capture YOU choose: Copy / Save / Copy+Save / Edit / OCR / Share / Open / Delete
- Editor: Pen, Highlighter, Arrow, Rectangle, Circle, Line, Text (size/bold/color), Blur, Pixelate, Crop, Resize, Undo/Redo, Eraser + colors + Thin/Medium/Thick
- OCR local via Tesseract (lazy-loaded, never at startup): English + Arabic, Auto/English/Arabic, Copy/Save/Copy+Save text, `Select Area → OCR → Copy → Done` without saving image
- History (optional, local only, thumbnails, limit 10/25/50/100/Disabled), per-item Copy/Open/Edit/OCR/Save/Delete
- Naming `OpenScreen_2026-09-22_23-14-05.png`, save folder picker, JPG/PNG, delay, presets (Copy Only / Save Only / Copy+Save / Edit / OCR)

## Requirements
- Windows 10/11, WebView2 (preinstalled on Win11)
- Rust stable (MSVC) + Node 18+
- For release builds: Visual Studio Build Tools (C++ workload)

## Native recording (no downloads)
Recording, replay and audio run on Windows itself (Graphics Capture + Media
Foundation + WASAPI) — no recorder downloads, no bundled recorder binaries.
Only OCR data ships with the installer:
- `src-tauri/resources/tesseract/` (Tesseract + `tessdata/eng` + `tessdata/ara`)
- Tesseract runtime fallback remains via winget if a custom build ships
  without `resources/`.

## Dev
```powershell
npm install
npm run tauri dev
```

## Build
```powershell
npm run tauri build
```

## Notes
- Startup loads only tray + hotkey listener. OCR/history load on demand.
- Temp OCR/screenshot files are deleted after use; closing the editor frees image memory.
- No cloud, no analytics on screenshot content. Everything local.
