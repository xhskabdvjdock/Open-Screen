import { invoke } from "@tauri-apps/api/core";

export interface AppSettings {
  startWithWindows: boolean;
  runInBackground: boolean;
  showTrayIcon: boolean;
  screenshotHotkey: string;
  ocrHotkey: string;
  defaultFormat: string;
  saveLocation: string;
  autoSave: boolean;
  autoCopy: boolean;
  captureCursor: boolean;
  delayMs: number;
  historyEnabled: boolean;
  historyLimit: number;
  ocrEnabled: boolean;
  ocrLanguage: string;
  ocrAuto: boolean;
  ocrAutoCopyText: boolean;
  defaultPenColor: string;
  defaultStroke: number;
  defaultFontSize: number;
  namingFormat: string;
  recStartHotkey: string;
  recStopHotkey: string;
  recPauseHotkey: string;
  recResumeHotkey: string;
  replaySaveHotkey: string;
  recDefaultSource: string;
  recMonitor: number;
  recLastArea: { x: number; y: number; w: number; h: number } | null;
  recFps: number;
  recResolution: string;
  recCustomW: number;
  recCustomH: number;
  recPreset: string;
  recQuality: string;
  recBitrate: string;
  recBitrateCustom: number;
  recCodec: string;
  recAudio: string;
  recMicDevice: string;
  recSampleRate: number;
  recChannels: number;
  recCursor: boolean;
  recFolder: string;
  recPostAction: string;
  recHistoryLimit: number;
  recPowerSaving: boolean;
  replayEnabled: boolean;
  replayAutostart: boolean;
  replayDuration: number;
  replayPreset: string;
  replayAudio: string;
  replayPostAction: string;
}

export const defaultSettings: AppSettings = {
  startWithWindows: false,
  runInBackground: true,
  showTrayIcon: true,
  screenshotHotkey: "Ctrl+Shift+S",
  ocrHotkey: "Ctrl+Shift+O",
  defaultFormat: "png",
  saveLocation: "",
  autoSave: false,
  autoCopy: false,
  captureCursor: false,
  delayMs: 0,
  historyEnabled: true,
  historyLimit: 50,
  ocrEnabled: true,
  ocrLanguage: "auto",
  ocrAuto: false,
  ocrAutoCopyText: false,
  defaultPenColor: "#e81123",
  defaultStroke: 3,
  defaultFontSize: 18,
  namingFormat: "OpenScreen_{date}_{time}",
  recStartHotkey: "Ctrl+Shift+R",
  recStopHotkey: "Ctrl+Shift+R",
  recPauseHotkey: "Ctrl+Shift+P",
  recResumeHotkey: "Ctrl+Shift+P",
  replaySaveHotkey: "Ctrl+Shift+I",
  recDefaultSource: "fullscreen",
  recMonitor: 0,
  recLastArea: null,
  recFps: 60,
  recResolution: "source",
  recCustomW: 1920,
  recCustomH: 1080,
  recPreset: "balanced",
  recQuality: "high",
  recBitrate: "auto",
  recBitrateCustom: 12,
  recCodec: "auto",
  recAudio: "none",
  recMicDevice: "",
  recSampleRate: 48000,
  recChannels: 2,
  recCursor: true,
  recFolder: "",
  recPostAction: "nothing",
  recHistoryLimit: 25,
  recPowerSaving: false,
  replayEnabled: false,
  replayAutostart: false,
  replayDuration: 30,
  replayPreset: "standard",
  replayAudio: "system",
  replayPostAction: "nothing",
};

export async function loadSettings(): Promise<AppSettings> {
  try {
    const s = await invoke<AppSettings>("settings_load");
    return { ...defaultSettings, ...s };
  } catch {
    return { ...defaultSettings };
  }
}

export async function saveSettings(s: AppSettings): Promise<void> {
  await invoke("settings_save", { s });
  await invoke("hotkeys_apply", { s }).catch(() => {});
}

let toastTimer: number | undefined;
export function toast(msg: string): void {
  let el = document.querySelector(".toast") as HTMLElement | null;
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add("show");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => el!.classList.remove("show"), 2200);
}

export function dataUrlToBase64(dataUrlOrB64: string): string {
  const i = dataUrlOrB64.indexOf("base64,");
  return i >= 0 ? dataUrlOrB64.slice(i + 7) : dataUrlOrB64;
}

export function base64ToDataUrl(b64: string, mime = "image/png"): string {
  if (b64.startsWith("data:")) return b64;
  return `data:${mime};base64,${b64}`;
}

export function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error("Failed to load image"));
    img.src = src;
  });
}

/** Release large image strings when done (memory req). */
export function releaseImageRef(obj: { base64?: string | null }): void {
  if (obj) obj.base64 = null;
}
