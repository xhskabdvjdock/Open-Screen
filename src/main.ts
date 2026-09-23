// Hidden main window: bootstrap only. Tray + hotkeys live in Rust.
// Keeps startup lightweight: no OCR preload, no history preload.
import { invoke } from "@tauri-apps/api/core";
import { loadSettings } from "./common";

async function boot() {
  try {
    const s = await loadSettings();
    await invoke("hotkeys_apply", { s }).catch(() => {});
  } catch { /* tray still works */ }
}
void boot();
