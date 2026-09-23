import { invoke } from "@tauri-apps/api/core";
import { listen, emit } from "@tauri-apps/api/event";
import { getCurrentWindow, PhysicalPosition, PhysicalSize } from "@tauri-apps/api/window";
import { icon } from "./icons";

interface MonitorInfo { index: number; name: string; x: number; y: number; width: number; height: number; scale: number; }
interface ActiveWin { title: string; x: number; y: number; width: number; height: number; }
interface StartPayload {
  mode?: string;
  bg?: string;
  bgWidth?: number;
  bgHeight?: number;
  monitors?: MonitorInfo[];
  activeWindow?: ActiveWin | null;
}

const canvas = document.getElementById("overlay") as HTMLCanvasElement;
const ctx = canvas.getContext("2d")!;
const dimsEl = document.getElementById("dims") as HTMLElement;
const hintText = document.getElementById("hint-text") as HTMLElement;

let monitors: MonitorInfo[] = [];
let activeWin: ActiveWin | null = null;
// Capture mode chosen from the small top-center pill: image vs text (OCR) vs record.
type CapMode = "image" | "text" | "record";
let captureMode: CapMode = "image";
const btnImg = document.getElementById("mode-image") as HTMLButtonElement;
const btnTxt = document.getElementById("mode-text") as HTMLButtonElement;
const btnRec = document.getElementById("mode-record") as HTMLButtonElement;
btnImg.innerHTML = `${icon("camera")}<span>Screenshot</span>`;
btnTxt.innerHTML = `${icon("scanText")}<span>Text</span>`;
btnRec.innerHTML = `<span class="recdot"></span><span>Record</span>`;
function setMode(m: CapMode) {
  captureMode = m;
  btnImg.classList.toggle("active", m === "image");
  btnTxt.classList.toggle("active", m === "text");
  btnRec.classList.toggle("active", m === "record");
  hintText.textContent =
    m === "record"
      ? "Select area to record · Tab switches mode · Esc cancel"
      : m === "text"
        ? "Frozen preview — select area for OCR · Tab switches mode · Esc cancel"
        : "Drag to select · Tab: Screenshot/Text/Record · F fullscreen · W window · A all · Esc cancel";
}
function cycleMode() {
  setMode(captureMode === "image" ? "text" : captureMode === "text" ? "record" : "image");
}
btnImg.onclick = (e) => { e.stopPropagation(); setMode("image"); };
btnTxt.onclick = (e) => { e.stopPropagation(); setMode("text"); };
btnRec.onclick = (e) => { e.stopPropagation(); setMode("record"); };
// Virtual-screen origin/size in physical pixels
let vX = 0, vY = 0, vW = 0, vH = 0;
let dpr = window.devicePixelRatio || 1;
// Frozen full-screen background (released from memory after selection)
let bgImg: HTMLImageElement | null = null;
let bgB64: string | null = null;
let bgW = 0, bgH = 0;
let start: { x: number; y: number } | null = null;
let cur: { x: number; y: number } | null = null;

function resize() {
  dpr = window.devicePixelRatio || 1;
  canvas.width = Math.floor(window.innerWidth * dpr);
  canvas.height = Math.floor(window.innerHeight * dpr);
  draw();
}

function virtualBounds(ms: MonitorInfo[]) {
  const xs = ms.map((m) => m.x);
  const ys = ms.map((m) => m.y);
  const xe = ms.map((m) => m.x + m.width);
  const ye = ms.map((m) => m.y + m.height);
  vX = Math.min(...xs); vY = Math.min(...ys);
  vW = Math.max(...xe) - vX; vH = Math.max(...ye) - vY;
}

function draw() {
  const w = canvas.width, h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  if (bgImg) {
    // Frozen full-screen preview, dimmed
    ctx.drawImage(bgImg, 0, 0, w, h);
    ctx.fillStyle = "rgba(0,0,0,.45)";
    ctx.fillRect(0, 0, w, h);
  }
  if (start && cur) {
    const x0 = Math.min(start.x, cur.x) * dpr;
    const y0 = Math.min(start.y, cur.y) * dpr;
    const x1 = Math.max(start.x, cur.x) * dpr;
    const y1 = Math.max(start.y, cur.y) * dpr;
    const rw = x1 - x0, rh = y1 - y0;
    if (bgImg) {
      // Punch a hole: redraw the frozen image region at full brightness
      const sx = (x0 / w) * bgW, sy = (y0 / h) * bgH;
      const sw = (rw / w) * bgW, sh = (rh / h) * bgH;
      ctx.drawImage(bgImg, sx, sy, sw, sh, x0, y0, rw, rh);
    } else {
      ctx.clearRect(x0, y0, rw, rh);
    }
    ctx.strokeStyle = "#ffffff";
    ctx.lineWidth = 2 * dpr;
    ctx.strokeRect(x0, y0, rw, rh);
    ctx.strokeStyle = "#0b57d0";
    ctx.lineWidth = 1 * dpr;
    ctx.strokeRect(x0, y0, rw, rh);
    const label = `${Math.round(rw / dpr)} × ${Math.round(rh / dpr)}`;
    dimsEl.hidden = false;
    dimsEl.textContent = label;
    ctx.font = `${12 * dpr}px "Segoe UI", sans-serif`;
    const tw = ctx.measureText(label).width + 16 * dpr;
    const lx = x0;
    let ly = y0 - 26 * dpr;
    if (ly < 0) ly = y1 + 6 * dpr;
    ctx.fillStyle = "rgba(11,87,208,.95)";
    ctx.fillRect(lx, ly, tw, 22 * dpr);
    ctx.fillStyle = "#fff";
    ctx.fillText(label, lx + 8 * dpr, ly + 15 * dpr);
  } else {
    dimsEl.hidden = true;
  }
}

/** Crop a rectangle (in frozen-background pixels) out of the frozen screen. */
function cropBg(rx: number, ry: number, rw: number, rh: number): { b64: string; w: number; h: number } | null {
  if (!bgImg || !bgB64) return null;
  const x = Math.max(0, Math.round(rx));
  const y = Math.max(0, Math.round(ry));
  const w = Math.min(bgW - x, Math.round(rw));
  const h = Math.min(bgH - y, Math.round(rh));
  if (w < 2 || h < 2) return null;
  if (x === 0 && y === 0 && w === bgW && h === bgH) {
    return { b64: bgB64, w: bgW, h: bgH }; // whole screen: reuse original bytes
  }
  const off = document.createElement("canvas");
  off.width = w; off.height = h;
  off.getContext("2d")!.drawImage(bgImg, x, y, w, h, 0, 0, w, h);
  const url = off.toDataURL("image/png");
  return { b64: url.slice(url.indexOf("base64,") + 7), w, h };
}

function releaseBg() {
  bgImg = null;
  bgB64 = null;
  bgW = 0; bgH = 0;
}

async function closeOverlay() {
  start = cur = null;
  releaseBg();
  draw();
  await getCurrentWindow().hide().catch(() => {});
}

async function finishSelection(crop: { b64: string; w: number; h: number } | null) {
  if (!crop) {
    start = cur = null;
    draw();
    return;
  }
  start = cur = null;
  await getCurrentWindow().hide().catch(() => {});
  releaseBg();
  await emit("openscreen:show-image", {
    base64: crop.b64, mime: "image/png", width: crop.w, height: crop.h,
    source: "screenshot", ocrMode: captureMode === "text",
  });
}

async function toastMsg(msg: string) {
  const { toast } = await import("./common");
  toast(msg);
}

async function finishRegion() {
  if (!start || !cur || !bgImg) {
    start = cur = null;
    draw();
    return;
  }
  // Map CSS pixels -> frozen-background pixels (DPI-safe)
  const cssW = canvas.clientWidth || window.innerWidth;
  const cssH = canvas.clientHeight || window.innerHeight;
  const sx = bgW / cssW, sy = bgH / cssH;
  if (captureMode === "record") {
    // Virtual-screen coords for the recorder (bg pixels == virtual pixels).
    const vx = vX + Math.min(start.x, cur.x) * sx;
    const vy = vY + Math.min(start.y, cur.y) * sy;
    const vw = Math.abs(cur.x - start.x) * sx;
    const vh = Math.abs(cur.y - start.y) * sy;
    if (vw < 16 || vh < 16) {
      start = cur = null;
      draw();
      return;
    }
    await startAreaRecording(vx, vy, vw, vh);
    return;
  }
  const x0 = Math.min(start.x, cur.x) * sx;
  const y0 = Math.min(start.y, cur.y) * sy;
  const rw = Math.abs(cur.x - start.x) * sx;
  const rh = Math.abs(cur.y - start.y) * sy;
  if (rw < 4 || rh < 4) {
    start = cur = null;
    draw();
    return;
  }
  await finishSelection(cropBg(x0, y0, rw, rh));
}

/** Countdown overlay (3-2-1) before recording. Shown on the picker window
 *  BEFORE capture starts, so it never appears in the final video.
 *  Esc cancels (returns false, no recording). */
let countdownCancelled = false;
let countdownShowing = false;
/** Hotkey-path countdown driven by the backend (Rust shows this window,
 *  emits `openscreen:countdown`, then hides it before capture starts). */
let hotkeyCountdown = false;
function ensureCountdownEl(): HTMLElement {
  let el = document.getElementById("countdown");
  if (!el) {
    el = document.createElement("div");
    el.id = "countdown";
    // Fullscreen stage so the count sits in the middle of the screen.
    // Never part of any capture: recording starts only after it finishes
    // and this window hides.
    el.style.cssText =
      "position:fixed;inset:0;display:none;align-items:center;justify-content:center;" +
      "font-size:130px;font-weight:800;color:#fff;background:rgba(0,0,0,.55);z-index:9999;" +
      "font-family:'Segoe UI',system-ui,sans-serif;user-select:none;";
    document.body.appendChild(el);
  }
  return el;
}
/** Hotkey path: transparent stage (no dim), banner only. */
function setHotkeyStage(on: boolean) {
  hotkeyCountdown = on;
  canvas.style.display = on ? "none" : "block";
  const hint = document.getElementById("hint");
  if (hint) hint.style.display = on ? "none" : "flex";
  document.body.style.background = on ? "transparent" : "";
}
function showBanner(text: string) {
  const el = ensureCountdownEl();
  el.textContent = text;
  el.style.display = "flex";
}
function hideBanner() {
  const el = document.getElementById("countdown");
  if (el) el.style.display = "none";
}
async function loadCountdownSecs(): Promise<number> {
  try {
    const { loadSettings } = await import("./common");
    const s = await loadSettings();
    const v = (s as unknown as Record<string, unknown>).recCountdown;
    const n = typeof v === "number" ? v : 3;
    return n === 0 || n === 3 || n === 5 || n === 10 ? n : 3;
  } catch {
    return 3;
  }
}
async function runCountdown(): Promise<boolean> {
  const secs = await loadCountdownSecs();
  if (!secs) return true; // Off: start immediately
  ensureCountdownEl();
  countdownCancelled = false;
  countdownShowing = true;
  start = cur = null;
  draw();
  for (let i = secs; i >= 1; i--) {
    if (countdownCancelled) {
      hideBanner();
      countdownShowing = false;
      return false;
    }
    showBanner(`⬤ ${i}`);
    await new Promise((r) => setTimeout(r, 1000));
  }
  if (countdownCancelled) {
    hideBanner();
    countdownShowing = false;
    return false;
  }
  showBanner("⬤ Recording");
  await new Promise((r) => setTimeout(r, 500));
  hideBanner();
  countdownShowing = false;
  return true;
}

/** Start a screen recording of a virtual-screen rect. Overlay hides first. */
async function startAreaRecording(vx: number, vy: number, w: number, h: number) {
  if (w < 16 || h < 16) {
    await toastMsg("Area too small to record");
    start = cur = null;
    draw();
    return;
  }
  // Countdown BEFORE hiding (visible to user, never in video since capture
  // starts only after it finishes).
  const go = await runCountdown();
  if (!go) {
    start = cur = null;
    await closeOverlay();
    return;
  }
  start = cur = null;
  await getCurrentWindow().hide().catch(() => {});
  releaseBg();
  draw();
  try {
    await invoke("rec_start_area", {
      x: Math.round(vx),
      y: Math.round(vy),
      w: Math.round(w),
      h: Math.round(h),
    });
    await toastMsg("Recording started — Ctrl+Shift+R to stop");
  } catch (e) {
    await toastMsg(typeof e === "string" ? e : "Unable to start screen recording.");
  }
}

async function captureSpecial(kind: "fullscreen" | "window" | "all") {
  if (!bgImg) {
    await toastMsg("Screen preview not ready — try again");
    return;
  }
  // Recording mode: F/W/A start a recording of that area.
  if (captureMode === "record") {
    if (kind === "all") {
      await startAreaRecording(vX, vY, vW, vH);
      return;
    }
    if (kind === "fullscreen") {
      const m = monitors[0];
      if (!m) {
        await startAreaRecording(vX, vY, vW, vH);
        return;
      }
      await startAreaRecording(m.x, m.y, m.width, m.height);
      return;
    }
    let aw = activeWin;
    if (!aw) {
      try {
        aw = await invoke<ActiveWin>("active_window_info");
      } catch {
        aw = null;
      }
    }
    if (!aw) {
      await toastMsg("No active window detected");
      start = cur = null;
      draw();
      return;
    }
    await startAreaRecording(aw.x, aw.y, aw.width, aw.height);
    return;
  }
  if (kind === "all") {
    await finishSelection({ b64: bgB64!, w: bgW, h: bgH });
    return;
  }
  if (kind === "fullscreen") {
    const m = monitors[0];
    if (!m) {
      await finishSelection({ b64: bgB64!, w: bgW, h: bgH });
      return;
    }
    await finishSelection(cropBg(m.x - vX, m.y - vY, m.width, m.height));
    return;
  }
  // Active window (captured before the overlay stole focus)
  let aw = activeWin;
  if (!aw) {
    try {
      aw = await invoke<ActiveWin>("active_window_info");
    } catch {
      aw = null;
    }
  }
  if (!aw) {
    await toastMsg("No active window detected");
    start = cur = null;
    draw();
    return;
  }
  await finishSelection(cropBg(aw.x - vX, aw.y - vY, aw.width, aw.height));
}

function pos(e: MouseEvent): { x: number; y: number } {
  const r = canvas.getBoundingClientRect();
  return { x: e.clientX - r.left, y: e.clientY - r.top };
}

canvas.addEventListener("mousedown", (e) => {
  if (hotkeyCountdown) return;
  if (e.button !== 0 || !bgImg) return;
  start = pos(e);
  cur = { ...start };
  draw();
});
window.addEventListener("mousemove", (e) => {
  if (!start) return;
  cur = pos(e);
  draw();
});
window.addEventListener("mouseup", () => { void finishRegion(); });
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    // Hotkey path first: must notify the backend, not just stop the banner.
    if (hotkeyCountdown) void cancelHotkeyCountdown();
    else if (countdownShowing) countdownCancelled = true;
    else void closeOverlay();
    return;
  }
  if (hotkeyCountdown) return; // banner owns the stage: ignore F/W/A/Tab/Enter
  if (e.key === "f" || e.key === "F") void captureSpecial("fullscreen");
  else if (e.key === "w" || e.key === "W") void captureSpecial("window");
  else if (e.key === "a" || e.key === "A") void captureSpecial("all");
  else if (e.key === "Enter") void captureSpecial("fullscreen");
  else if (e.key === "Tab") { e.preventDefault(); cycleMode(); }
});
window.addEventListener("resize", resize);

async function fitVirtualScreen(ms: MonitorInfo[]) {
  if (!ms.length) {
    try {
      ms = await invoke<MonitorInfo[]>("monitors");
    } catch {
      ms = [];
    }
  }
  monitors = ms;
  if (monitors.length) virtualBounds(monitors);
  const win = getCurrentWindow();
  try {
    await win.setFullscreen(false);
    if (monitors.length) {
      await win.setPosition(new PhysicalPosition(vX, vY));
      await win.setSize(new PhysicalSize(Math.max(vW, 800), Math.max(vH, 600)));
    }
  } catch {
    /* single-monitor fallback */
  }
  resize();
}

function loadBgImage(b64: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error("preview failed"));
    img.src = `data:image/png;base64,${b64}`;
  });
}

async function cancelHotkeyCountdown() {
  countdownCancelled = true;
  try {
    await invoke("rec_countdown_cancel");
  } catch { /* backend reads the flag regardless */ }
  hideBanner();
  setHotkeyStage(false);
  await getCurrentWindow().hide().catch(() => {});
}

/** Backend-driven countdown (hotkey/tray path): top-center banner, Esc cancels. */
async function onBackendCountdown(secs: number) {
  if (!secs) return;
  countdownCancelled = false;
  countdownShowing = true;
  setHotkeyStage(true);
  start = cur = null;
  for (let i = secs; i >= 1; i--) {
    if (countdownCancelled) break;
    showBanner(`⬤ ${i}`);
    await new Promise((r) => setTimeout(r, 1000));
  }
  if (!countdownCancelled) showBanner("⬤ Recording");
}

async function init() {
  resize();
  // Stay hidden at startup — only shown via capture-start with a frozen background.
  await getCurrentWindow().hide().catch(() => {});
  await listen<{ secs?: number }>("openscreen:countdown", async (ev) => {
    await fitVirtualScreen(monitors);
    void onBackendCountdown(ev.payload?.secs ?? 3);
  });
  await listen("openscreen:countdown-hide", async () => {
    countdownShowing = false;
    hideBanner();
    setHotkeyStage(false);
  });
  await listen<StartPayload>("openscreen:capture-start", async (ev) => {
    const p = ev.payload ?? {};
    // A fresh picker run cancels any stale hotkey-countdown stage.
    countdownCancelled = true;
    countdownShowing = false;
    hideBanner();
    setHotkeyStage(false);
    const m = p.mode ?? "region";
    setMode(m === "ocr" ? "text" : m === "record" ? "record" : "image");
    start = cur = null;
    releaseBg();
    monitors = p.monitors ?? [];
    activeWin = p.activeWindow ?? null;
    if (p.bg && p.bgWidth && p.bgHeight) {
      try {
        bgB64 = p.bg;
        bgW = p.bgWidth;
        bgH = p.bgHeight;
        bgImg = await loadBgImage(p.bg);
      } catch {
        releaseBg();
      }
    }
    await fitVirtualScreen(monitors);
    draw();
  });
  await fitVirtualScreen([]);
}

void init();
