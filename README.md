# Open Screen — Screenshot Tool + Lightweight Image Editor + Local OCR

**Capture fast. Edit when needed. OCR when needed. Save only when needed.**

Lightweight, local-first Windows desktop app built with **Rust + Tauri** (no Electron).

## Features
- System tray (Take Screenshot / OCR / History / Settings / Quit), no always-open window
- Global hotkeys: `Ctrl+Shift+S` screenshot, `Ctrl+Shift+O` OCR (both remappable)
- Capture mode pill (top-center): Screenshot / Text / **Record** — `Tab` cycles, one shortcut for all
- Screen recording (ffmpeg, local, lazy one-time download): area/fullscreen/monitor/all-screens/window, pause/resume (lossless concat), MP4 H.264, hardware encoding first (NVENC/QuickSync/AMF → software fallback with notice), mic/system/mixed audio, cursor toggle
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

## Bundled engines (install-time, offline)
The installer ships everything — no runtime downloads needed:
- `src-tauri/resources/ffmpeg/ffmpeg.exe` (screen recording, HW encoders included)
- `src-tauri/resources/tesseract/` (Tesseract + `tessdata/eng` + `tessdata/ara`)
- Runtime downloads (winget / GitHub) remain only as a fallback if a custom
  build is made without `resources/`.
- Note: the installer is large (~100MB+) because the engines travel with it.
  ffmpeg is GPL (used as a separate process, not linked).

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
