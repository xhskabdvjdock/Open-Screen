import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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
    if (!isAllowedPair) warn.push(`“${v}” is used by ${ks.join(", ")}`);
  }
  (document.getElementById("hotkeyWarn") as HTMLElement).textContent =
    warn.length ? "Shortcut conflict: " + warn.join(" · ") : "";
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
      o.textContent = `Monitor ${m.index + 1} (${m.width}×${m.height})`;
      src.appendChild(o);
      const o2 = document.createElement("option");
      o2.value = String(m.index);
      o2.textContent = `Monitor ${m.index + 1} — ${m.name}`;
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
    toast(on ? "Instant Replay buffering…" : "Instant Replay off");
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
      ? `Ready — buffering last ${st.replayDuration}s`
      : st.replay === "starting" ? "Starting…" : st.replay === "saving" ? "Saving…" : "Off";
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
    }>(force ? "rec_probe_refresh" : "rec_probe");
    const hw = [...p.h264, ...p.hevc, ...p.av1].find((e) => e.includes("nvenc") || e.includes("qsv") || e.includes("amf"));
    label.textContent = `${p.version.split(" ").slice(0, 3).join(" ")} · HW: ${hw ?? "software only"}`;
    // Codec dropdown: only list supported families (never fake).
    const codec = document.getElementById("recCodec") as HTMLSelectElement;
    [...codec.options].forEach((o) => {
      if (o.value === "hevc") o.hidden = p.hevc.length === 0;
      if (o.value === "av1") o.hidden = !(p.av1.some((e) => e.includes("nvenc") || e.includes("qsv") || e.includes("amf")));
    });
    (document.getElementById("codecNote") as HTMLElement).textContent =
      `Active encoders: H.264 [${p.h264.join(", ") || "—"}]` +
      (p.hevc.length ? ` · HEVC [${p.hevc.join(", ")}]` : "") +
      ` · Default H.264 for compatibility.`;
    // Mic devices: only real devices.
    const mic = document.getElementById("recMicDevice") as HTMLSelectElement;
    mic.innerHTML = "";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = p.audioDevices.length ? "Default microphone" : "No microphone found";
    mic.appendChild(def);
    p.audioDevices.forEach((d) => {
      const o = document.createElement("option");
      o.value = d; o.textContent = d;
      mic.appendChild(o);
    });
    mic.value = settings.recMicDevice;
    if (!p.audioDevices.length) {
      toast("Audio capture is unavailable — video will record without audio.");
    }
  } catch (e) {
    label.textContent = "Recorder engine missing.";
    (document.getElementById("ffmpegDl") as HTMLButtonElement).classList.add("primary");
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
  (document.getElementById("ffmpegDl") as HTMLButtonElement).onclick = async () => {
    const btn = document.getElementById("ffmpegDl") as HTMLButtonElement;
    btn.disabled = true;
    (document.getElementById("ffmpegProg") as HTMLElement).classList.remove("hidden");
    try {
      const m = await invoke<string>("ffmpeg_ensure");
      toast(m);
    } catch (e) { toast(typeof e === "string" ? e : "Download failed"); }
    finally {
      btn.disabled = false;
      (document.getElementById("ffmpegProg") as HTMLElement).classList.add("hidden");
      await refreshProbe(true);
    }
  };
  await listen<{ pct: number }>("openscreen:ffmpeg-progress", (ev) => {
    (document.getElementById("ffmpegPct") as HTMLElement).textContent = `${ev.payload.pct}%`;
  });
  (document.getElementById("recPerfRefresh") as HTMLButtonElement).onclick = async () => {
    try {
      const st = await invoke<{
        recording: boolean; paused: boolean; elapsedS: number; frames: number; fps: number;
        width: number; height: number; encoderLabel: string; sizeBytes: number; dropped: number;
        sysDb: number | null; micDb: number | null; message: string;
      }>("rec_status");
      const bar = (db: number | null) => {
        if (db == null) return "—";
        const n = Math.max(0, Math.min(10, Math.round((db + 60) / 6)));
        return "█".repeat(n) + "░".repeat(10 - n) + ` ${db.toFixed(0)}dB`;
      };
      (document.getElementById("recPerf") as HTMLElement).textContent =
        (st.message ? st.message + " · " : "") +
        (st.recording || st.paused
          ? `${st.encoderLabel} · ${st.width}×${st.height}@${st.fps} · ${st.frames} frames · ${st.elapsedS}s · ${(st.sizeBytes / 1048576).toFixed(1)}MB · dropped ~${st.dropped} · sys ${bar(st.sysDb)} · mic ${bar(st.micDb)}`
          : "Idle. Start a recording to see live stats.");
    } catch { /* ignore */ }
  };
  (document.getElementById("ocrCheck") as HTMLButtonElement).onclick = async () => {
    try {
      const s = await invoke<{ available: boolean; engine: string; langs: string[] }>("ocr_status");
      (document.getElementById("ocrEngineLabel") as HTMLElement).textContent = s.available
        ? `${s.engine} — ${s.langs.join(", ") || "ready"}`
        : "Not installed. Use Editor → OCR → Install Tesseract.";
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
      await invoke("hotkeys_apply", { s: next }).catch(() => toast("Hotkey not applied — check format like Ctrl+Shift+S"));
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
