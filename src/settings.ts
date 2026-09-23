import { invoke } from "@tauri-apps/api/core";

import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as dialogOpen } from "@tauri-apps/plugin-dialog";
import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
import { loadSettings, toast, type AppSettings } from "./common";

const TABS = ["general", "screenshot", "ocr", "editor", "history", "recording", "replay", "shortcuts", "appearance", "about"];
let settings: AppSettings;

function showTab(name: string) {
  document.querySelectorAll("section").forEach((s) => s.classList.toggle("active", s.getAttribute("data-tab") === name));
  document.querySelectorAll("#tabs button").forEach((b) => b.classList.toggle("active", b.textContent?.toLowerCase() === name));
}
function buildTabs() {
  const nav = document.getElementById("tabs")!;
  nav.innerHTML = "";
  for (const t of TABS) {
    const b = document.createElement("button");
    b.className = "btn";
    b.textContent = t[0].toUpperCase() + t.slice(1);
    b.onclick = () => showTab(t);
    nav.appendChild(b);
  }
}
const val = (id: string) => (document.getElementById(id) as HTMLInputElement).value;
const checked = (id: string) => (document.getElementById(id) as HTMLInputElement).checked;
const setVal = (id: string, v: string) => ((document.getElementById(id) as HTMLInputElement).value = v);
const setChecked = (id: string, v: boolean) => ((document.getElementById(id) as HTMLInputElement).checked = v);

function checkHotkeyConflicts() {
  const keys = ["screenshotHotkey","ocrHotkey","recStartHotkey","recStopHotkey","recPauseHotkey","recResumeHotkey","replaySaveHotkey"];
  const norm = (s: string) => s.trim().toLowerCase();
  const seen = new Map<string, string[]>();
  for (const k of keys) {
    const v = norm(val(k));
    if (!v) continue;
    if (!seen.has(v)) seen.set(v, []);
    seen.get(v)!.push(k);
  }
  const allowed: [string,string][] = [["recStartHotkey","recStopHotkey"],["recPauseHotkey","recResumeHotkey"]];
  const warn: string[] = [];
  for (const [v, ks] of seen) {
    if (ks.length < 2) continue;
    const isAllowedPair = ks.length === 2 && allowed.some(([a,b]) =>
      (ks.includes(a) && ks.includes(b)));
    if (!isAllowedPair) warn.push(`â€œ${v}â€ is used by ${ks.join(", ")}`);
  }
  (document.getElementById("hotkeyWarn") as HTMLElement).textContent =
    warn.length ? "Shortcut conflict: " + warn.join(" Â· ") : "";
}

async function fillMonitors(selected: number) {
  const sel = document.getElementById("recMonitor") as HTMLSelectElement;
  sel.innerHTML = "";
  try {
    const ms = await invoke<{ index: number; name: string; width: number; height: number }[]>("monitors");
    const src = document.getElementById("recDefaultSource") as HTMLSelectElement;
    // Add per-monitor sources dynamically
    [...src.options].forEach((o) => { if (o.value.startsWith("monitor:")) o.remove(); });
    ms.forEach((m) => {
      const o = document.createElement("option");
      o.value = `monitor:${m.index}`;
      o.textContent = `Monitor ${m.index + 1} (${m.width}Ã—${m.height})`;
      src.appendChild(o);
      const o2 = document.createElement("option");
      o2.value = String(m.index);
      o2.textContent = `Monitor ${m.index + 1} â€” ${m.name}`;
      sel.appendChild(o2);
    });
    sel.value = String(selected);
  } catch { /* keep empty */ }
}

function replayDurationValue(): number {
  const sel = (document.getElementById("replayDuration") as HTMLSelectElement).value;
  if (sel === "custom") {
    const n = parseInt((document.getElementById("replayCustom") as HTMLInputElement).value) || 30;
    return Math.min(600, Math.max(5, n));
  }
  return parseInt(sel) || 30;
}

function syncReplayDurationUI() {
  const sel = (document.getElementById("replayDuration") as HTMLSelectElement).value;
  const v = sel === "custom"
    ? parseInt((document.getElementById("replayCustom") as HTMLInputElement).value) || 30
    : parseInt(sel) || 30;
  (document.getElementById("replayWarn") as HTMLElement).textContent =
    v > 60 ? "Longer replay duration may increase memory and disk usage." : "";
}

async function onReplayToggle() {
  const on = (document.getElementById("replayEnabled") as HTMLInputElement).checked;
  try {
    await invoke(on ? "replay_start" : "replay_stop");
    settings.replayEnabled = on;
    toast(on ? "Instant Replay bufferingâ€¦" : "Instant Replay off");
  } catch (e) {
    (document.getElementById("replayEnabled") as HTMLInputElement).checked = !on;
    toast(typeof e === "string" ? e : "Failed to toggle replay");
  }
  void refreshReplayStatus();
}

async function refreshReplayStatus() {
  try {
    const st = await invoke<{ replay: string; replayReadyS: number; replayDuration: number }>("rec_status");
    const label = st.replay === "ready"
      ? `Ready â€” buffering last ${st.replayDuration}s`
      : st.replay === "starting" ? "Startingâ€¦" : st.replay === "saving" ? "Savingâ€¦" : "Off";
    (document.getElementById("replayStatus") as HTMLElement).textContent = label;
    (document.getElementById("replayEnabled") as HTMLInputElement).checked = st.replay !== "off";
  } catch { /* ignore */ }
}

async function refreshProbe(force: boolean) {
  const label = document.getElementById("ffmpegLabel") as HTMLElement;
  try {
    const p = await invoke<{
      ffmpeg: string; version: string; h264: string[]; hevc: string[]; av1: string[];
      audioDevices: string[]; systemHint: string | null;
      captureApi: string; hasDdagrab: boolean;
      wasapiMics: string[]; wasapiSystem: string | null;
    }>(force ? "rec_probe_refresh" : "rec_probe");
    const isHw = (n: string) => !/microsoft/i.test(n);
    const hw = [...p.h264, ...p.hevc].find(isHw);
    label.textContent = `Native engine (WGC + Media Foundation + WASAPI) Â· ${hw ? `HW: ${hw}` : "Software encoding"}`;
    // Codec dropdown: only list families with a real MFT (never fake).
    const codec = document.getElementById("recCodec") as HTMLSelectElement;
    [...codec.options].forEach((o) => {
      if (o.value === "hevc") o.hidden = p.hevc.length === 0;
      if (o.value === "av1") o.hidden = true; // no inbox AV1 encoder MFT
    });
    (document.getElementById("codecNote") as HTMLElement).textContent =
      `Active encoders: H.264 [${p.h264.join(", ") || "â€”"}]` +
      (p.hevc.length ? ` Â· HEVC [${p.hevc.join(", ")}]` : "") +
      ` Â· Default H.264 for compatibility.`;
    // Capture backend: exactly one â€” Windows Graphics Capture.
    try {
      const native = await invoke<{ backend: string; monitors: { name: string; width: number; height: number; refreshRate: number }[] }>("native_probe");
      (document.getElementById("captureApiLabel") as HTMLElement).textContent =
        `${native.backend} live (${native.monitors.length} display${native.monitors.length === 1 ? "" : "s"}: ` +
        native.monitors.map((m) => `${m.width}Ã—${m.height}@${m.refreshRate || "?"}Hz`).join(", ") + ").";
    } catch {
      (document.getElementById("captureApiLabel") as HTMLElement).textContent =
        `${p.captureApi || "Windows Graphics Capture"} â€” verified at recording start.`;
    }
    // Mic devices: real WASAPI only. Never fake.
    const pool = p.wasapiMics;
    const mic = document.getElementById("recMicDevice") as HTMLSelectElement;
    mic.innerHTML = "";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = pool.length ? "Default microphone" : "No microphone found";
    mic.appendChild(def);
    pool.forEach((d) => {
      const o = document.createElement("option");
      o.value = d; o.textContent = d;
      mic.appendChild(o);
    });
    mic.value = settings.recMicDevice;
    (document.getElementById("micListNote") as HTMLElement).textContent =
      p.wasapiMics.length
        ? `Real WASAPI devices (${p.wasapiMics.length}).`
        : "No microphone found.";
    (document.getElementById("sysAudioLabel") as HTMLElement).textContent =
      p.wasapiSystem
        ? `WASAPI loopback: ${p.wasapiSystem}`
        : "No system-audio endpoint found â€” video only.";
  } catch (e) {
    label.textContent = "Native engine unavailable (requires Windows 10 1803+).";
    (document.getElementById("ffmpegDl") as HTMLButtonElement).classList.add("primary");
  }
}

function levelBar(db: number | null): string {
  if (db == null) return "â–‘â–‘â–‘â–‘â–‘â–‘â–‘â–‘â–‘â–‘ --dB";
  const n = Math.max(0, Math.min(10, Math.round((db + 60) / 6)));
  return "â–ˆ".repeat(n) + "â–‘".repeat(10 - n) + ` ${db.toFixed(0)}dB`;
}

async function refreshLevels() {
  try {
    const st = await invoke<{
      recording: boolean; paused: boolean; sysDb: number | null; micDb: number | null;
      audioStatus: string;
    }>("rec_status");
    (document.getElementById("audioLevels") as HTMLElement).textContent =
      `sys ${levelBar(st.sysDb)} Â· mic ${levelBar(st.micDb)} (${st.audioStatus})`;
  } catch {
    (document.getElementById("audioLevels") as HTMLElement).textContent = "Levels unavailable.";
  }
}

async function init() {
  buildTabs();
  showTab("general");
  settings = await loadSettings();
  setChecked("startWithWindows", settings.startWithWindows);
  setChecked("runInBackground", settings.runInBackground);
  setVal("screenshotHotkey", settings.screenshotHotkey);
  setVal("ocrHotkey", settings.ocrHotkey);
  setVal("defaultFormat", settings.defaultFormat);
  setVal("saveLocation", settings.saveLocation);
  (document.getElementById("saveLocationLabel") as HTMLElement).textContent = settings.saveLocation;
  setChecked("autoSave", settings.autoSave);
  setChecked("autoCopy", settings.autoCopy);
  setVal("delayMs", String(settings.delayMs));
  setVal("namingFormat", settings.namingFormat);
  setChecked("ocrEnabled", settings.ocrEnabled);
  setVal("ocrLanguage", settings.ocrLanguage);
  setChecked("ocrAuto", settings.ocrAuto);
  setChecked("ocrAutoCopyText", settings.ocrAutoCopyText);
  setVal("defaultPenColor", settings.defaultPenColor);
  setVal("defaultStroke", String(settings.defaultStroke));
  setVal("defaultFontSize", String(settings.defaultFontSize));
  setChecked("historyEnabled", settings.historyEnabled);
  setVal("historyLimit", settings.historyLimit === 0 ? "0" : String(settings.historyLimit));
  // recording hotkeys
  setVal("recStartHotkey", settings.recStartHotkey);
  setVal("recStopHotkey", settings.recStopHotkey);
  setVal("recPauseHotkey", settings.recPauseHotkey);
  setVal("recResumeHotkey", settings.recResumeHotkey);
  setVal("replaySaveHotkey", settings.replaySaveHotkey);
  for (const id of ["screenshotHotkey","ocrHotkey","recStartHotkey","recStopHotkey","recPauseHotkey","recResumeHotkey","replaySaveHotkey"]) {
    (document.getElementById(id) as HTMLInputElement).oninput = checkHotkeyConflicts;
  }
  checkHotkeyConflicts();
  // recording
  setVal("recDefaultSource", settings.recDefaultSource);
  await fillMonitors(settings.recMonitor);
  setVal("recFolder", settings.recFolder);
  (document.getElementById("recFolderLabel") as HTMLElement).textContent = settings.recFolder;
  setChecked("recCursor", settings.recCursor);
  setVal("recPostAction", settings.recPostAction);
  setVal("recHistoryLimit", String(settings.recHistoryLimit));
  setChecked("recPowerSaving", settings.recPowerSaving);
  setVal("recPreset", settings.recPreset);
  setVal("recResolution", settings.recResolution);
  setVal("recCustomW", String(settings.recCustomW));
  setVal("recCustomH", String(settings.recCustomH));
  setVal("recFps", String(settings.recFps));
  setVal("recQuality", settings.recQuality);
  setVal("recBitrate", settings.recBitrate);
  setVal("recBitrateCustom", String(settings.recBitrateCustom));
  setVal("recCodec", settings.recCodec);
  setVal("recHwMode", settings.recHwMode || "auto");
  setVal("recCountdown", String(settings.recCountdown ?? 3));
  setVal("recAudio", settings.recAudio);
  setVal("recSampleRate", String(settings.recSampleRate));
  setVal("recChannels", String(settings.recChannels));
  // replay
  setChecked("replayEnabled", settings.replayEnabled);
  setChecked("replayAutostart", settings.replayAutostart);
  (document.getElementById("replayEnabled") as HTMLInputElement).onchange = onReplayToggle;
  syncReplayDurationUI();
  (document.getElementById("replayDuration") as HTMLSelectElement).onchange = syncReplayDurationUI;
  (document.getElementById("replayCustom") as HTMLInputElement).value = String(
    [10,15,30,60,90,120,300].includes(settings.replayDuration) ? 30 : settings.replayDuration
  );
  setVal("replayPreset", settings.replayPreset);
  setVal("replayAudio", settings.replayAudio);
  setVal("replayPostAction", settings.replayPostAction);
  (document.getElementById("replaySaveNow") as HTMLButtonElement).onclick = async () => {
    try {
      await invoke("replay_save");
      toast("Replay saved");
    } catch (e) { toast(typeof e === "string" ? e : "Replay save failed"); }
    void refreshReplayStatus();
  };
  await refreshProbe(false);
  await refreshReplayStatus();
  try {
    if (await isEnabled()) setChecked("startWithWindows", true);
  } catch { /* ignore */ }

  (document.getElementById("pickFolder") as HTMLButtonElement).onclick = async () => {
    const dir = await dialogOpen({ directory: true }).catch(() => null);
    if (dir && typeof dir === "string") { setVal("saveLocation", dir); (document.getElementById("saveLocationLabel") as HTMLElement).textContent = dir; }
  };
  (document.getElementById("pickRecFolder") as HTMLButtonElement).onclick = async () => {
    const dir = await dialogOpen({ directory: true }).catch(() => null);
    if (dir && typeof dir === "string") { setVal("recFolder", dir); (document.getElementById("recFolderLabel") as HTMLElement).textContent = dir; }
  };
  (document.getElementById("ffmpegCheck") as HTMLButtonElement).onclick = async () => {
    await refreshProbe(true);
    toast("Probe refreshed");
  };
  // Native engine: nothing to download â€” the button re-verifies backends.
  (document.getElementById("ffmpegDl") as HTMLButtonElement).onclick = async () => {
    const btn = document.getElementById("ffmpegDl") as HTMLButtonElement;
    btn.disabled = true;
    try {
      const m = await invoke<string>("ffmpeg_ensure");
      toast(m);
    } catch (e) { toast(typeof e === "string" ? e : "Engine check failed"); }
    finally {
      btn.disabled = false;
      await refreshProbe(true);
    }
  };
  const refreshPerf = async () => {
    try {
      const st = await invoke<{
        recording: boolean; paused: boolean; elapsedS: number; frames: number; fps: number;
        actualFps: number; width: number; height: number; encoderLabel: string; captureApi: string;
        sizeBytes: number; dropped: number; duplicated: number; sysDb: number | null; micDb: number | null;
        audioStatus: string; perfWarning: string | null; message: string; powerNote: string | null;
      }>("rec_status");
      let cpu = "";
      try {
        const px = await invoke<{ sysCpu: number; appCpu: number }>("perf_extra");
        cpu = ` Â· CPU sys ${px.sysCpu.toFixed(0)}% / app ${px.appCpu.toFixed(0)}%`;
      } catch { /* CPU optional */ }
      const bar = (db: number | null) => {
        if (db == null) return "â€”";
        const n = Math.max(0, Math.min(10, Math.round((db + 60) / 6)));
        return "â–ˆ".repeat(n) + "â–‘".repeat(10 - n) + ` ${db.toFixed(0)}dB`;
      };
      (document.getElementById("recPerf") as HTMLElement).textContent =
        (st.message ? st.message + " Â· " : "") +
        (st.recording || st.paused
          ? `${st.encoderLabel} Â· capture ${st.captureApi || "auto"} Â· ${st.width}Ã—${st.height} target ${st.fps} / actual ~${Math.round(st.actualFps)} Â· ${st.frames} frames Â· dropped ~${st.dropped} Â· duplicated ~${st.duplicated} Â· ${st.elapsedS}s Â· ${(st.sizeBytes / 1048576).toFixed(1)}MB Â· ${st.audioStatus} Â· sys ${bar(st.sysDb)} Â· mic ${bar(st.micDb)}${cpu}` +
            (st.perfWarning ? ` Â· ${st.perfWarning}` : "") +
            (st.powerNote ? ` Â· ${st.powerNote}` : "")
          : "Idle. Start a recording to see live stats.");
    } catch { /* ignore */ }
  };
  (document.getElementById("recPerfRefresh") as HTMLButtonElement).onclick = () => void refreshPerf();
  (document.getElementById("recPerfRefresh2") as HTMLButtonElement).onclick = () => void refreshPerf();
  (document.getElementById("audioLevelsRefresh") as HTMLButtonElement).onclick = () => void refreshLevels();
  (document.getElementById("audioRefresh") as HTMLButtonElement).onclick = async () => {
    await refreshProbe(true);
    toast("Audio devices refreshed");
  };
  (document.getElementById("testMic") as HTMLButtonElement).onclick = async () => {
    const btn = document.getElementById("testMic") as HTMLButtonElement;
    btn.disabled = true;
    (document.getElementById("audioLevels") as HTMLElement).textContent = "mic listeningâ€¦ speak now";
    try {
      const mic = (document.getElementById("recMicDevice") as HTMLSelectElement).value || "";
      const db = await invoke<number | null>("audio_level_test", { kind: "mic", mic });
      (document.getElementById("audioLevels") as HTMLElement).textContent =
        `mic ${levelBar(db)}` + (db == null ? " â€” silent or unavailable. Check the device." : "");
    } catch {
      (document.getElementById("audioLevels") as HTMLElement).textContent = "mic test failed.";
    } finally {
      btn.disabled = false;
    }
  };
  (document.getElementById("testSysAudio") as HTMLButtonElement).onclick = async () => {
    const btn = document.getElementById("testSysAudio") as HTMLButtonElement;
    btn.disabled = true;
    (document.getElementById("audioLevels") as HTMLElement).textContent = "system listeningâ€¦ play something";
    try {
      const db = await invoke<number | null>("audio_level_test", { kind: "system", mic: "" });
      (document.getElementById("audioLevels") as HTMLElement).textContent =
        `sys ${levelBar(db)}` + (db == null ? " â€” silent. Play audio and retry." : "");
    } catch {
      (document.getElementById("audioLevels") as HTMLElement).textContent = "system test failed.";
    } finally {
      btn.disabled = false;
    }
  };
  (document.getElementById("ocrCheck") as HTMLButtonElement).onclick = async () => {
    try {
      const s = await invoke<{ available: boolean; engine: string; langs: string[] }>("ocr_status");
      (document.getElementById("ocrEngineLabel") as HTMLElement).textContent = s.available
        ? `${s.engine} â€” ${s.langs.join(", ") || "ready"}`
        : "Not installed. Use Editor â†’ OCR â†’ Install Tesseract.";
      toast(s.available ? "OCR engine ready" : "OCR engine missing");
    } catch { toast("OCR check failed"); }
  };
  (document.getElementById("clearHistory") as HTMLButtonElement).onclick = async () => {
    await invoke("history_clear").catch(() => {});
    toast("History cleared");
  };
  (document.getElementById("preset") as HTMLSelectElement).onchange = (e) => {
    const p = (e.target as HTMLSelectElement).value;
    if (p === "copy") { setChecked("autoCopy", true); setChecked("autoSave", false); }
    else if (p === "save") { setChecked("autoCopy", false); setChecked("autoSave", true); }
    else if (p === "copysave") { setChecked("autoCopy", true); setChecked("autoSave", true); }
    else if (p === "manual") { setChecked("autoCopy", false); setChecked("autoSave", false); }
    else if (p === "ocr") { setChecked("ocrAuto", true); }
    toast(p === "manual" ? "Manual mode" : `Preset: ${p}`);
  };
  (document.getElementById("btn-close") as HTMLButtonElement).onclick = async () => {
    await getCurrentWindow().hide().catch(() => {});
  };
  (document.getElementById("btn-save") as HTMLButtonElement).onclick = async () => {
    const next: AppSettings = {
      ...settings,
      startWithWindows: checked("startWithWindows"),
      runInBackground: checked("runInBackground"),
      screenshotHotkey: val("screenshotHotkey") || "Ctrl+Shift+S",
      ocrHotkey: val("ocrHotkey") || "Ctrl+Shift+O",
      defaultFormat: val("defaultFormat"),
      saveLocation: val("saveLocation"),
      autoSave: checked("autoSave"),
      autoCopy: checked("autoCopy"),
      delayMs: parseInt(val("delayMs")) || 0,
      namingFormat: val("namingFormat") || "OpenScreen_{date}_{time}",
      ocrEnabled: checked("ocrEnabled"),
      ocrLanguage: val("ocrLanguage"),
      ocrAuto: checked("ocrAuto"),
      ocrAutoCopyText: checked("ocrAutoCopyText"),
      defaultPenColor: val("defaultPenColor"),
      defaultStroke: parseInt(val("defaultStroke")) || 4,
      defaultFontSize: parseInt(val("defaultFontSize")) || 18,
      historyEnabled: checked("historyEnabled"),
      historyLimit: parseInt(val("historyLimit")) || 0,
      captureCursor: settings.captureCursor,
      showTrayIcon: settings.showTrayIcon,
      recStartHotkey: val("recStartHotkey") || "Ctrl+Shift+R",
      recStopHotkey: val("recStopHotkey") || "Ctrl+Shift+R",
      recPauseHotkey: val("recPauseHotkey") || "Ctrl+Shift+P",
      recResumeHotkey: val("recResumeHotkey") || "Ctrl+Shift+P",
      replaySaveHotkey: val("replaySaveHotkey") || "Ctrl+Shift+I",
      recDefaultSource: val("recDefaultSource"),
      recMonitor: parseInt(val("recMonitor")) || 0,
      recLastArea: settings.recLastArea,
      recFps: parseInt(val("recFps")) || 60,
      recResolution: val("recResolution"),
      recCustomW: parseInt(val("recCustomW")) || 1920,
      recCustomH: parseInt(val("recCustomH")) || 1080,
      recPreset: val("recPreset"),
      recQuality: val("recQuality"),
      recBitrate: val("recBitrate"),
      recBitrateCustom: parseInt(val("recBitrateCustom")) || 12,
      recCodec: val("recCodec"),
      recHwMode: val("recHwMode") || "auto",
      recCountdown: [0, 3, 5, 10].includes(parseInt(val("recCountdown"))) ? parseInt(val("recCountdown")) : 3,
      recAudio: val("recAudio"),
      recMicDevice: val("recMicDevice"),
      recSampleRate: parseInt(val("recSampleRate")) || 48000,
      recChannels: parseInt(val("recChannels")) || 2,
      recCursor: checked("recCursor"),
      recFolder: val("recFolder"),
      recPostAction: val("recPostAction"),
      recHistoryLimit: parseInt(val("recHistoryLimit")) || 0,
      recPowerSaving: checked("recPowerSaving"),
      replayEnabled: checked("replayEnabled"),
      replayAutostart: checked("replayAutostart"),
      replayDuration: replayDurationValue(),
      replayPreset: val("replayPreset"),
      replayAudio: val("replayAudio"),
      replayPostAction: val("replayPostAction"),
    };
    try {
      await invoke("settings_save", { s: next });
      try {
        const failed = await invoke<string[]>("hotkeys_apply", { s: next });
        if (failed.length) toast("Shortcut already in use: " + failed.join(" Â· "));
      } catch {
        toast("Hotkey not applied â€” check format like Ctrl+Shift+S");
      }
      if (next.startWithWindows) { await enable().catch(() => {}); } else { await disable().catch(() => {}); }
      settings = next;
      toast("Settings saved");
      await getCurrentWindow().hide().catch(() => {});
    } catch (e) {
      toast(typeof e === "string" ? e : "Failed to save settings");
    }
  };
}
void init();
