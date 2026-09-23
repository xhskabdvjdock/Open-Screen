import { invoke } from "@tauri-apps/api/core";
import { listen, emit } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { save as dialogSave, open as dialogOpen } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";
import { icon } from "./icons";
import { loadSettings, toast, base64ToDataUrl, dataUrlToBase64, loadImage, type AppSettings } from "./common";

// ---------- State ----------
type Tool = "select" | "pen" | "highlighter" | "arrow" | "rect" | "circle" | "line" | "text" | "blur" | "pixelate" | "crop" | "eraser";
interface Pt { x: number; y: number; }
interface ShapeBase { id: string; tool: Tool; color: string; size: number; }
interface StrokeShape extends ShapeBase { tool: "pen" | "highlighter"; points: Pt[]; }
interface BoxShape extends ShapeBase { tool: "rect" | "blur" | "pixelate"; x: number; y: number; w: number; h: number; }
interface CircleShape extends ShapeBase { tool: "circle"; x: number; y: number; w: number; h: number; }
interface LineShape extends ShapeBase { tool: "line" | "arrow"; x0: number; y0: number; x1: number; y1: number; }
interface TextShape extends ShapeBase { tool: "text"; x: number; y: number; text: string; fontSize: number; bold: boolean; align: CanvasTextAlign; }
type Shape = StrokeShape | BoxShape | CircleShape | LineShape | TextShape;

let settings: AppSettings;
let baseImg: HTMLImageElement | null = null;
let baseW = 0, baseH = 0;
let currentBase64: string | null = null;
let currentMime = "image/png";
let shapes: Shape[] = [];
let tool: Tool = "select";
let color = "#e81123";
let strokeSize = 4;
let zoom = 1;
let savedPath: string | null = null;
let ocrModeInitial = false;
let drawing: { start: Pt; pts?: Pt[] } | null = null;
let cropRect: { x: number; y: number; w: number; h: number } | null = null;

interface Snapshot { base: string | null; shapes: string; }
let undoStack: Snapshot[] = [];
let redoStack: Snapshot[] = [];

// ---------- DOM ----------
const $ = (id: string) => document.getElementById(id) as HTMLElement;
const canvas = $("canvas") as HTMLCanvasElement;
const ctx = canvas.getContext("2d")!;
const wrap = $("canvas-wrap") as HTMLElement;

const TOOLS: { id: Tool; label: string; ic: string }[] = [
  { id: "select", label: "Select", ic: "arrow" },
  { id: "pen", label: "Pen", ic: "pencil" },
  { id: "highlighter", label: "Highlighter", ic: "highlighter" },
  { id: "arrow", label: "Arrow", ic: "arrow" },
  { id: "rect", label: "Rectangle", ic: "square" },
  { id: "circle", label: "Circle", ic: "circle" },
  { id: "line", label: "Line", ic: "line" },
  { id: "text", label: "Text", ic: "type" },
  { id: "blur", label: "Blur", ic: "blur" },
  { id: "pixelate", label: "Pixelate", ic: "pixelate" },
  { id: "crop", label: "Crop", ic: "crop" },
  { id: "eraser", label: "Eraser", ic: "eraser" },
];
const COLORS = ["#e81123", "#0b57d0", "#16a34a", "#eab308", "#111111", "#ffffff"];

function setIcon(id: string, name: string) {
  const el = $(id).querySelector(".ic") as HTMLElement;
  if (el) el.innerHTML = icon(name);
}

function buildToolbar() {
  setIcon("btn-copy", "copy"); setIcon("btn-save", "save"); setIcon("btn-copysave", "save");
  setIcon("btn-ocr", "scanText"); setIcon("btn-edit", "pencil"); setIcon("btn-share", "share");
  setIcon("btn-more", "more"); setIcon("btn-cancel", "x");
  setIcon("btn-undo", "undo"); setIcon("btn-redo", "redo"); setIcon("btn-ocr-close", "x");
  const tg = $("tools");
  tg.innerHTML = "";
  for (const t of TOOLS) {
    const b = document.createElement("button");
    b.className = "icon-btn" + (tool === t.id ? " active" : "");
    b.title = t.label;
    b.innerHTML = icon(t.ic);
    b.onclick = () => setTool(t.id);
    tg.appendChild(b);
  }
  const cc = $("colors");
  cc.innerHTML = "";
  for (const c of COLORS) {
    const s = document.createElement("div");
    s.className = "swatch" + (color === c ? " active" : "");
    s.title = c;
    s.style.background = c;
    s.onclick = () => { color = c; ($("custom-color") as HTMLInputElement).value = c; buildToolbar(); };
    cc.appendChild(s);
  }
}

function setTool(t: Tool) {
  tool = t;
  cropRect = null;
  $("crop-opts").classList.toggle("hidden", t !== "crop");
  $("text-opts").classList.toggle("hidden", t !== "text");
  $("resize-opts").classList.add("hidden");
  canvas.style.cursor = t === "select" ? "default" : t === "text" ? "text" : "crosshair";
  buildToolbar();
}

// ---------- Render ----------
function snapshot(): Snapshot { return { base: currentBase64, shapes: JSON.stringify(shapes) }; }
function pushUndo() {
  undoStack.push(snapshot());
  if (undoStack.length > 40) undoStack.shift();
  redoStack = [];
}
async function restore(s: Snapshot) {
  currentBase64 = s.base;
  shapes = JSON.parse(s.shapes) as Shape[];
  if (currentBase64) {
    baseImg = await loadImage(base64ToDataUrl(currentBase64, currentMime));
    baseW = baseImg.naturalWidth; baseH = baseImg.naturalHeight;
  } else baseImg = null;
  render();
}
async function undo() {
  if (!undoStack.length) return;
  redoStack.push(snapshot());
  await restore(undoStack.pop()!);
}
async function redo() {
  if (!redoStack.length) return;
  undoStack.push(snapshot());
  await restore(redoStack.pop()!);
}

function render() {
  if (!baseImg) return;
  canvas.width = Math.round(baseW * zoom);
  canvas.height = Math.round(baseH * zoom);
  ctx.save();
  ctx.scale(zoom, zoom);
  ctx.clearRect(0, 0, baseW, baseH);
  ctx.drawImage(baseImg, 0, 0, baseW, baseH);
  for (const s of shapes) drawShape(s);
  // draft
  if (drawing && tool !== "select" && tool !== "text" && tool !== "eraser") {
    drawDraft(drawing.start, lastPt);
  }
  if (cropRect) {
    ctx.save();
    ctx.fillStyle = "rgba(0,0,0,.45)";
    const { x, y, w, h } = cropRect;
    ctx.fillRect(0, 0, baseW, y); ctx.fillRect(0, y + h, baseW, baseH - y - h);
    ctx.fillRect(0, y, x, h); ctx.fillRect(x + w, y, baseW - x - w, h);
    ctx.strokeStyle = "#fff"; ctx.lineWidth = 1.5;
    ctx.strokeRect(x, y, w, h);
    ctx.restore();
  }
  ctx.restore();
  $("st-dims").textContent = `${baseW} × ${baseH}`;
  $("st-zoom").textContent = `${Math.round(zoom * 100)}%`;
}

function drawShape(s: Shape) {
  ctx.save();
  ctx.strokeStyle = s.color; ctx.fillStyle = s.color;
  ctx.lineWidth = s.tool === "highlighter" ? s.size * 3 : s.size;
  ctx.lineCap = "round"; ctx.lineJoin = "round";
  if (s.tool === "pen" || s.tool === "highlighter") {
    ctx.globalAlpha = s.tool === "highlighter" ? 0.45 : 1;
    ctx.beginPath();
    s.points.forEach((p, i) => (i === 0 ? ctx.moveTo(p.x, p.y) : ctx.lineTo(p.x, p.y)));
    ctx.stroke();
  } else if (s.tool === "rect") {
    ctx.strokeRect(s.x, s.y, s.w, s.h);
  } else if (s.tool === "circle") {
    ctx.beginPath(); ctx.ellipse(s.x + s.w / 2, s.y + s.h / 2, Math.abs(s.w / 2), Math.abs(s.h / 2), 0, 0, Math.PI * 2); ctx.stroke();
  } else if (s.tool === "line" || s.tool === "arrow") {
    ctx.beginPath(); ctx.moveTo(s.x0, s.y0); ctx.lineTo(s.x1, s.y1); ctx.stroke();
    if (s.tool === "arrow") {
      const ang = Math.atan2(s.y1 - s.y0, s.x1 - s.x0);
      const L = 10 + s.size * 2;
      ctx.beginPath();
      ctx.moveTo(s.x1, s.y1);
      ctx.lineTo(s.x1 - L * Math.cos(ang - 0.42), s.y1 - L * Math.sin(ang - 0.42));
      ctx.moveTo(s.x1, s.y1);
      ctx.lineTo(s.x1 - L * Math.cos(ang + 0.42), s.y1 - L * Math.sin(ang + 0.42));
      ctx.stroke();
    }
  } else if (s.tool === "text") {
    ctx.globalAlpha = 1;
    ctx.font = `${s.bold ? "bold " : ""}${s.fontSize}px "Segoe UI", sans-serif`;
    ctx.textAlign = s.align;
    ctx.fillText(s.text, s.x, s.y);
  }
  // blur/pixelate are committed to base raster (not live shapes) — nothing to draw here.
  ctx.restore();
}

let lastPt: Pt | null = null;
function drawDraft(a: Pt, b: Pt | null) {
  if (!b) return;
  ctx.save();
  ctx.strokeStyle = color; ctx.fillStyle = color;
  ctx.lineWidth = (tool === "highlighter" ? strokeSize * 3 : strokeSize);
  ctx.globalAlpha = tool === "highlighter" ? 0.45 : 1;
  const x = Math.min(a.x, b.x), y = Math.min(a.y, b.y);
  const w = Math.abs(b.x - a.x), h = Math.abs(b.y - a.y);
  if (tool === "rect" || tool === "blur" || tool === "pixelate" || tool === "crop") ctx.strokeRect(x, y, w, h);
  else if (tool === "circle") { ctx.beginPath(); ctx.ellipse(x + w / 2, y + h / 2, w / 2, h / 2, 0, 0, Math.PI * 2); ctx.stroke(); }
  else if (tool === "line" || tool === "arrow") { ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke(); }
  ctx.restore();
}

function canvasPos(e: MouseEvent): Pt {
  const r = canvas.getBoundingClientRect();
  return { x: (e.clientX - r.left) / zoom, y: (e.clientY - r.top) / zoom };
}

// Commit blur/pixelate rect into base image (raster, undoable)
async function commitRasterRect(kind: "blur" | "pixelate", x: number, y: number, w: number, h: number) {
  if (w < 2 || h < 2 || !baseImg) return;
  pushUndo();
  const off = document.createElement("canvas");
  off.width = baseW; off.height = baseH;
  const octx = off.getContext("2d")!;
  octx.drawImage(baseImg, 0, 0);
  // draw existing vector shapes first so blur covers annotations too
  const tmp = document.createElement("canvas");
  tmp.width = baseW; tmp.height = baseH;
  const tctx = tmp.getContext("2d")!;
  tctx.drawImage(off, 0, 0);
  // apply effect on sub-rect
  const sx = Math.max(0, Math.floor(x)), sy = Math.max(0, Math.floor(y));
  const sw = Math.min(baseW - sx, Math.floor(w)), sh = Math.min(baseH - sy, Math.floor(h));
  if (kind === "blur") {
    const sub = document.createElement("canvas");
    sub.width = sw; sub.height = sh;
    const sctx = sub.getContext("2d")!;
    sctx.filter = "blur(14px)";
    sctx.drawImage(off, sx, sy, sw, sh, 0, 0, sw, sh);
    // second pass for strong privacy
    sctx.filter = "blur(14px)";
    sctx.drawImage(sub, 0, 0);
    octx.drawImage(sub, sx, sy);
  } else {
    const k = 14;
    const small = document.createElement("canvas");
    small.width = Math.max(1, Math.floor(sw / k)); small.height = Math.max(1, Math.floor(sh / k));
    const sm = small.getContext("2d")!;
    sm.imageSmoothingEnabled = false;
    sm.drawImage(off, sx, sy, sw, sh, 0, 0, small.width, small.height);
    octx.imageSmoothingEnabled = false;
    octx.drawImage(small, 0, 0, small.width, small.height, sx, sy, sw, sh);
    octx.imageSmoothingEnabled = true;
  }
  // flatten vector shapes into raster as well (so later edits stay consistent)
  currentBase64 = dataUrlToBase64(off.toDataURL("image/png"));
  baseImg = await loadImage(base64ToDataUrl(currentBase64));
  shapes = [];
  render();
}

// Flatten all (base + shapes) to PNG base64 for copy/save/share
function flattenedBase64(fmt: "png" | "jpg" = "png", quality = 0.92): string {
  const off = document.createElement("canvas");
  off.width = baseW; off.height = baseH;
  const octx = off.getContext("2d")!;
  if (fmt === "jpg") { octx.fillStyle = "#ffffff"; octx.fillRect(0, 0, baseW, baseH); }
  octx.drawImage(baseImg!, 0, 0);
  // redraw shapes at 1x
  const prevZoom = zoom;
  void prevZoom;
  const tmpCanvas = canvas;
  void tmpCanvas;
  // draw shapes manually
  const keepCtx = ctx;
  void keepCtx;
  const c2 = document.createElement("canvas");
  c2.width = baseW; c2.height = baseH;
  const g = c2.getContext("2d")!;
  g.drawImage(baseImg!, 0, 0);
  for (const s of shapes) {
    g.save();
    g.strokeStyle = s.color; g.fillStyle = s.color;
    g.lineWidth = s.tool === "highlighter" ? s.size * 3 : s.size;
    g.lineCap = "round"; g.lineJoin = "round";
    if (s.tool === "pen" || s.tool === "highlighter") {
      g.globalAlpha = s.tool === "highlighter" ? 0.45 : 1;
      g.beginPath();
      (s as StrokeShape).points.forEach((p, i) => (i === 0 ? g.moveTo(p.x, p.y) : g.lineTo(p.x, p.y)));
      g.stroke();
    } else if (s.tool === "rect") { const b = s as BoxShape; g.strokeRect(b.x, b.y, b.w, b.h); }
    else if (s.tool === "circle") { const b = s as CircleShape; g.beginPath(); g.ellipse(b.x + b.w / 2, b.y + b.h / 2, Math.abs(b.w / 2), Math.abs(b.h / 2), 0, 0, Math.PI * 2); g.stroke(); }
    else if (s.tool === "line" || s.tool === "arrow") {
      const l = s as LineShape;
      g.beginPath(); g.moveTo(l.x0, l.y0); g.lineTo(l.x1, l.y1); g.stroke();
      if (l.tool === "arrow") {
        const ang = Math.atan2(l.y1 - l.y0, l.x1 - l.x0);
        const L = 10 + l.size * 2;
        g.beginPath();
        g.moveTo(l.x1, l.y1);
        g.lineTo(l.x1 - L * Math.cos(ang - 0.42), l.y1 - L * Math.sin(ang - 0.42));
        g.moveTo(l.x1, l.y1);
        g.lineTo(l.x1 - L * Math.cos(ang + 0.42), l.y1 - L * Math.sin(ang + 0.42));
        g.stroke();
      }
    } else if (s.tool === "text") {
      const t = s as TextShape;
      g.font = `${t.bold ? "bold " : ""}${t.fontSize}px "Segoe UI", sans-serif`;
      g.textAlign = t.align;
      g.fillText(t.text, t.x, t.y);
    }
    g.restore();
  }
  octx.clearRect(0, 0, baseW, baseH);
  if (fmt === "jpg") { octx.fillStyle = "#ffffff"; octx.fillRect(0, 0, baseW, baseH); }
  octx.drawImage(c2, 0, 0);
  return dataUrlToBase64(off.toDataURL(fmt === "jpg" ? "image/jpeg" : "image/png", quality));
}

// ---------- Mouse interactions ----------
canvas.addEventListener("mousedown", (e) => {
  if (!baseImg || e.button !== 0) return;
  const p = canvasPos(e);
  if (tool === "text") { openTextInput(p); return; }
  if (tool === "eraser") { eraseAt(p); return; }
  if (tool === "select") return;
  drawing = { start: p };
  lastPt = p;
});
canvas.addEventListener("mousemove", (e) => {
  if (!drawing || !baseImg) return;
  const p = canvasPos(e);
  lastPt = p;
  if (tool === "pen" || tool === "highlighter") {
    drawing.pts = [...(drawing.pts ?? [drawing.start]), p];
    // live draw incremental
    render();
    ctx.save(); ctx.scale(zoom, zoom);
    ctx.strokeStyle = color; ctx.lineWidth = tool === "highlighter" ? strokeSize * 3 : strokeSize;
    ctx.globalAlpha = tool === "highlighter" ? 0.45 : 1;
    ctx.lineCap = "round";
    ctx.beginPath();
    const pts = drawing.pts;
    pts.forEach((q, i) => (i === 0 ? ctx.moveTo(q.x, q.y) : ctx.lineTo(q.x, q.y)));
    ctx.stroke(); ctx.restore();
  } else {
    render();
  }
});
canvas.addEventListener("mouseup", async (e) => {
  if (!drawing || !baseImg) { drawing = null; return; }
  const p = canvasPos(e);
  const a = drawing.start, b = p;
  const w = Math.abs(b.x - a.x), h = Math.abs(b.y - a.y);
  const id = Math.random().toString(36).slice(2);
  if (tool === "pen" || tool === "highlighter") {
    const pts = drawing.pts ?? [a, b];
    if (pts.length > 1) { pushUndo(); shapes.push({ id, tool, points: pts, color, size: strokeSize } as StrokeShape); }
  } else if (tool === "rect" || tool === "blur" || tool === "pixelate") {
    if (w > 3 && h > 3) {
      const x = Math.min(a.x, b.x), y = Math.min(a.y, b.y);
      if (tool === "rect") { pushUndo(); shapes.push({ id, tool, x, y, w, h, color, size: strokeSize } as BoxShape); }
      else await commitRasterRect(tool, x, y, w, h);
    }
  } else if (tool === "circle") {
    if (w > 3 && h > 3) { pushUndo(); shapes.push({ id, tool, x: Math.min(a.x, b.x), y: Math.min(a.y, b.y), w, h, color, size: strokeSize } as CircleShape); }
  } else if (tool === "line" || tool === "arrow") {
    if (Math.hypot(b.x - a.x, b.y - a.y) > 4) { pushUndo(); shapes.push({ id, tool, x0: a.x, y0: a.y, x1: b.x, y1: b.y, color, size: strokeSize } as LineShape); }
  } else if (tool === "crop") {
    if (w > 4 && h > 4) cropRect = { x: Math.min(a.x, b.x), y: Math.min(a.y, b.y), w, h };
  }
  drawing = null; lastPt = null;
  render();
});

function eraseAt(p: Pt) {
  // Remove topmost shape containing the point
  for (let i = shapes.length - 1; i >= 0; i--) {
    const s = shapes[i];
    if (shapeHit(s, p)) { pushUndo(); shapes.splice(i, 1); render(); return; }
  }
  toast("Nothing to erase here");
}
function shapeHit(s: Shape, p: Pt): boolean {
  const pad = 8;
  if (s.tool === "pen" || s.tool === "highlighter") {
    return (s as StrokeShape).points.some((q) => Math.hypot(q.x - p.x, q.y - p.y) < pad + s.size);
  }
  if (s.tool === "rect") { const b = s as BoxShape; return p.x >= b.x - pad && p.x <= b.x + b.w + pad && p.y >= b.y - pad && p.y <= b.y + b.h + pad; }
  if (s.tool === "circle") { const b = s as CircleShape; return p.x >= b.x - pad && p.x <= b.x + b.w + pad && p.y >= b.y - pad && p.y <= b.y + b.h + pad; }
  if (s.tool === "line" || s.tool === "arrow") {
    const l = s as LineShape;
    const d = distToSeg(p, { x: l.x0, y: l.y0 }, { x: l.x1, y: l.y1 });
    return d < pad + s.size;
  }
  if (s.tool === "text") { const t = s as TextShape; return Math.abs(p.x - t.x) < 120 && Math.abs(p.y - t.y) < t.fontSize + pad; }
  return false;
}
function distToSeg(p: Pt, a: Pt, b: Pt): number {
  const dx = b.x - a.x, dy = b.y - a.y;
  const len2 = dx * dx + dy * dy || 1;
  const t = Math.max(0, Math.min(1, ((p.x - a.x) * dx + (p.y - a.y) * dy) / len2));
  return Math.hypot(p.x - (a.x + t * dx), p.y - (a.y + t * dy));
}

// Text input
const textInput = $("text-input") as HTMLInputElement;
function openTextInput(p: Pt) {
  const r = canvas.getBoundingClientRect();
  textInput.classList.remove("hidden");
  textInput.style.left = `${r.left - wrap.getBoundingClientRect().left + p.x * zoom}px`;
  textInput.style.top = `${r.top - wrap.getBoundingClientRect().top + p.y * zoom}px`;
  textInput.style.color = color;
  textInput.style.fontSize = `${($("font-size") as HTMLInputElement).value}px`;
  textInput.value = "";
  textInput.focus();
  textInput.onkeydown = (e) => {
    if (e.key === "Enter" && textInput.value.trim()) {
      pushUndo();
      shapes.push({
        id: Math.random().toString(36).slice(2), tool: "text", x: p.x, y: p.y,
        text: textInput.value, color, size: strokeSize,
        fontSize: parseInt(($("font-size") as HTMLInputElement).value) || 18,
        bold: $("btn-bold").classList.contains("active"), align: "left",
      } as TextShape);
      textInput.classList.add("hidden");
      render();
    } else if (e.key === "Escape") textInput.classList.add("hidden");
  };
}

// ---------- Actions ----------
async function doCopy(): Promise<boolean> {
  if (!baseImg) return false;
  try {
    const b64 = flattenedBase64("png");
    await invoke("clipboard_copy_image", { base64Png: b64 });
    toast("Copied to clipboard");
    return true;
  } catch (e) {
    toast(typeof e === "string" ? e : "Unable to copy screenshot to clipboard.");
    return false;
  }
}

async function doSave(suggestName?: string): Promise<string | null> {
  if (!baseImg) return null;
  const fmt = settings.defaultFormat === "jpg" ? "jpg" : "png";
  const name = suggestName ?? await invoke<string>("generate_filename", { format: fmt, pattern: settings.namingFormat });
  const filters = fmt === "jpg"
    ? [{ name: "JPEG", extensions: ["jpg", "jpeg"] }]
    : [{ name: "PNG", extensions: ["png"] }];
  const dest = await dialogSave({ defaultPath: `${settings.saveLocation}/${name}`, filters }).catch(() => null);
  if (!dest) return null;
  try {
    let b64 = flattenedBase64(fmt === "jpg" ? "jpg" : "png");
    // for jpg quality path, convert via backend for better encoder
    if (fmt === "jpg") {
      const pngB64 = flattenedBase64("png");
      const out = await invoke<{ base64: string }>("convert_image", { base64Png: pngB64, format: "jpg", quality: 90 });
      b64 = out.base64;
    }
    await invoke("save_image_bytes", { base64Data: b64, path: dest });
    savedPath = dest;
    toast("Saved");
    return dest;
  } catch {
    toast("Unable to save screenshot. [Choose another folder]");
    return null;
  }
}

async function autoBehaviors() {
  if (!baseImg) return;
  if (settings.autoCopy) await doCopy().catch(() => {});
  if (settings.autoSave) {
    try {
      const fmt = settings.defaultFormat === "jpg" ? "jpg" : "png";
      const name = await invoke<string>("generate_filename", { format: fmt, pattern: settings.namingFormat });
      const dir = settings.saveLocation || await invoke<string>("default_save_dir");
      let b64 = flattenedBase64(fmt === "jpg" ? "jpg" : "png");
      if (fmt === "jpg") {
        const out = await invoke<{ base64: string }>("convert_image", { base64Png: b64, format: "jpg", quality: 90 });
        b64 = out.base64;
      }
      savedPath = `${dir}/${name}`;
      await invoke("save_image_bytes", { base64Data: b64, path: savedPath });
    } catch { /* silent: user chose manual flow */ }
  }
  if (settings.historyEnabled) {
    try {
      await invoke("history_add", { base64Png: flattenedBase64("png"), width: baseW, height: baseH, limit: settings.historyLimit });
    } catch { /* non-blocking */ }
  }
  if (settings.ocrAuto && settings.ocrEnabled) {
    // Never auto-run OCR while a recording/replay buffer is active —
    // keep the capture pipeline smooth; manual OCR still works on demand.
    try {
      const st = await invoke<{ recording: boolean; paused: boolean; replay: string }>("rec_status");
      if (st.recording || st.paused || st.replay !== "off") return;
    } catch { /* fall through to OCR */ }
    openOcrPanel();
    void runOcr();
  }
}

// ---------- OCR ----------
function openOcrPanel() {
  $("ocr-panel").classList.remove("hidden");
  ($("ocr-lang") as HTMLSelectElement).value = settings.ocrLanguage === "eng" ? "eng" : settings.ocrLanguage === "ara" ? "ara" : "auto";
  void refreshOcrStatus();
}
async function refreshOcrStatus() {
  const st = $("ocr-status");
  try {
    const s = await invoke<{ available: boolean; langs: string[]; has_eng: boolean; has_ara: boolean }>("ocr_status");
    $("ocr-install").classList.toggle("hidden", s.available);
    const sel = $("ocr-lang") as HTMLSelectElement;
    if (sel.options.length >= 3) {
      sel.options[1].text = s.has_eng ? "English" : "English (needs download)";
      sel.options[2].text = s.has_ara ? "Arabic" : "Arabic (needs download)";
    }
    // Language-data download box (Arabic/English traineddata, one-time)
    const dlBox = $("ocr-lang-dl");
    const needDl = s.available && (!s.has_ara || !s.has_eng);
    dlBox.classList.toggle("hidden", !needDl);
    if (needDl) {
      const what = !s.has_ara ? "ara" : "eng";
      ($("ocr-lang-msg") as HTMLElement).textContent = what === "ara"
        ? "Arabic OCR data is missing. Download once (~3MB), then Arabic works fully offline. / بيانات العربية غير موجودة — حمّلها مرة واحدة (~3MB) ثم تعمل دون إنترنت."
        : "English OCR data is missing. Download once, then it works fully offline.";
      const dlBtn = $("btn-ocr-dl") as HTMLButtonElement;
      dlBtn.textContent = what === "ara" ? "Download Arabic data" : "Download English data";
      dlBtn.onclick = () => void (async () => {
        dlBtn.disabled = true;
        st.textContent = "Downloading language data…";
        try {
          const m = await invoke<string>("ocr_ensure_lang", { lang: what });
          toast(m);
        } catch (e) {
          toast(typeof e === "string" ? e : "Download failed");
        } finally {
          dlBtn.disabled = false;
          void refreshOcrStatus();
        }
      })();
    }
    st.textContent = s.available
      ? `Local OCR ready (${s.langs.join(", ") || "eng"}). Nothing is uploaded.`
      : "OCR engine missing — install once, then works offline.";
  } catch { st.textContent = "OCR unavailable."; }
}
async function runOcr() {
  const btn = $("btn-ocr-run") as HTMLButtonElement;
  const out = $("ocr-text") as HTMLTextAreaElement;
  const st = $("ocr-status");
  if (!baseImg) return;
  btn.disabled = true;
  st.textContent = "Running OCR locally…";
  try {
    const lang = ($("ocr-lang") as HTMLSelectElement).value;
    const b64 = flattenedBase64("png");
    const r = await invoke<{ text: string; ms: number }>("ocr_image", { base64Png: b64, lang });
    out.value = r.text;
    out.dir = /[\u0600-\u06FF]/.test(r.text) ? "rtl" : "ltr";
    st.textContent = r.text.trim() ? `Done in ${r.ms}ms. Nothing uploaded.` : "No text detected. Try a clearer area.";
    if (settings.ocrAutoCopyText && r.text.trim()) {
      await invoke("clipboard_copy_text", { text: r.text }).catch(() => {});
    }
  } catch (e) {
    const msg = typeof e === "string" ? e : "OCR failed";
    if (msg.includes("OCR_ENGINE_MISSING")) {
      st.textContent = "OCR engine not installed.";
      $("ocr-install").classList.remove("hidden");
    } else if (msg.includes("OCR_LANG_MISSING")) {
      st.textContent = "Arabic/English data missing — connect once to download tessdata, then retry.";
    } else {
      st.textContent = "OCR failed. Try selecting a clearer area or choose another language.";
      const retry = document.createElement("button");
      retry.className = "btn"; retry.textContent = "Retry";
      retry.onclick = () => { retry.remove(); void runOcr(); };
      st.appendChild(document.createElement("br")); st.appendChild(retry);
    }
    toast("OCR failed");
  } finally { btn.disabled = false; }
}

// ---------- Menus ----------
function closeMenu() { $("menu-pop").classList.add("hidden"); }
function openMenu(anchor: HTMLElement, items: { label: string; fn: () => void }[]) {
  const pop = $("menu-pop");
  pop.innerHTML = "";
  for (const it of items) {
    const b = document.createElement("button");
    b.className = "btn"; b.textContent = it.label;
    b.onclick = () => { closeMenu(); it.fn(); };
    pop.appendChild(b);
  }
  const r = anchor.getBoundingClientRect();
  pop.style.top = `${r.bottom + 6}px`;
  pop.style.left = `${Math.min(r.left, window.innerWidth - 230)}px`;
  pop.classList.remove("hidden");
}
document.addEventListener("click", (e) => {
  const pop = $("menu-pop");
  if (!pop.classList.contains("hidden") && !(e.target as HTMLElement).closest("#menu-pop")
    && !(e.target as HTMLElement).closest("#btn-share") && !(e.target as HTMLElement).closest("#btn-more")) closeMenu();
});

// ---------- Wire up ----------
async function init() {
  settings = await loadSettings();
  color = settings.defaultPenColor || color;
  strokeSize = settings.defaultStroke || 4;
  ($("stroke") as HTMLSelectElement).value = String([2, 4, 8].includes(strokeSize) ? strokeSize : 4);
  ($("font-size") as HTMLInputElement).value = String(settings.defaultFontSize || 18);
  ($("custom-color") as HTMLInputElement).value = color;
  buildToolbar();
  setTool("select");
  $("edit-bar").classList.add("hidden");

  ($("stroke") as HTMLSelectElement).onchange = (e) => { strokeSize = parseInt((e.target as HTMLSelectElement).value); };
  ($("custom-color") as HTMLInputElement).oninput = (e) => { color = (e.target as HTMLInputElement).value; buildToolbar(); };
  $("btn-bold").onclick = () => $("btn-bold").classList.toggle("active");
  $("btn-undo").onclick = () => void undo();
  $("btn-redo").onclick = () => void redo();
  window.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z" && !e.shiftKey) { e.preventDefault(); void undo(); }
    else if ((e.ctrlKey || e.metaKey) && (e.key.toLowerCase() === "y" || (e.key.toLowerCase() === "z" && e.shiftKey))) { e.preventDefault(); void redo(); }
    else if (e.key === "Escape") {
      if (!$("ocr-panel").classList.contains("hidden")) $("ocr-panel").classList.add("hidden");
      else if (tool === "crop" && cropRect) { cropRect = null; render(); }
      else void closeEditor(true);
    }
  });

  $("btn-edit").onclick = () => { $("edit-bar").classList.toggle("hidden"); };
  $("btn-copy").onclick = () => void (async () => { if (await doCopy()) void closeEditor(false); })();
  $("btn-save").onclick = () => void (async () => { const p = await doSave(); if (p) void closeEditor(false); })();
  $("btn-copysave").onclick = () => void (async () => { const ok = await doCopy(); const p = await doSave(); if (ok || p) void closeEditor(false); })();
  $("btn-ocr").onclick = () => { const p = $("ocr-panel"); p.classList.toggle("hidden"); if (!p.classList.contains("hidden")) void refreshOcrStatus(); };
  $("btn-ocr-close").onclick = () => $("ocr-panel").classList.add("hidden");
  $("btn-ocr-run").onclick = () => void runOcr();
  $("btn-ocr-copy").onclick = () => void (async () => {
    const t = ($("ocr-text") as HTMLTextAreaElement).value;
    if (!t.trim()) { toast("No text to copy"); return; }
    // Backend clipboard first (works everywhere), browser API as fallback.
    try {
      await invoke("clipboard_copy_text", { text: t });
    } catch {
      try {
        await navigator.clipboard.writeText(t);
      } catch {
        toast("Unable to copy text to clipboard.");
        return;
      }
    }
    const n = t.trim().length;
    toast(`Copied ${n} character${n === 1 ? "" : "s"}`);
  })();
  $("btn-ocr-save").onclick = () => void (async () => {
    const t = ($("ocr-text") as HTMLTextAreaElement).value;
    if (!t.trim()) { toast("No text to save"); return; }
    const dest = await dialogSave({ defaultPath: "OpenScreen_ocr.txt", filters: [{ name: "Text", extensions: ["txt"] }] }).catch(() => null);
    if (!dest) return;
    // UTF-8 BOM so Arabic renders correctly in Notepad and other editors.
    const bytes = new Uint8Array([0xef, 0xbb, 0xbf, ...new TextEncoder().encode(t)]);
    let binary = ""; bytes.forEach((b: number) => (binary += String.fromCharCode(b)));
    await invoke("save_image_bytes", { base64Data: btoa(binary), path: dest });
    toast("Text saved");
  })();
  $("btn-ocr-copysave").onclick = () => void (async () => { ($("btn-ocr-copy") as HTMLButtonElement).click(); ($("btn-ocr-save") as HTMLButtonElement).click(); })();
  $("btn-ocr-install").onclick = () => void (async () => {
    $("ocr-status").textContent = "Installing Tesseract (one-time)…";
    try { const m = await invoke<string>("ocr_install"); toast(m); void refreshOcrStatus(); }
    catch (e) { toast(typeof e === "string" ? e : "Install failed. Install 'Tesseract OCR' manually."); }
  })();

  $("btn-share").onclick = (e) => openMenu(e.currentTarget as HTMLElement, [
    { label: "Copy image", fn: () => void doCopy() },
    { label: "Save as…", fn: () => void doSave() },
    {
      label: "Open with default app", fn: () => void (async () => {
        if (!baseImg) return;
        const tmp = `${await invoke<string>("default_save_dir")}/../Open Screen/tmp-share.png`;
        try {
          await invoke("save_image_bytes", { base64Data: flattenedBase64("png"), path: tmp });
          await invoke("open_path", { path: tmp });
        } catch { toast("Unable to open image"); }
      })(),
    },
    { label: "Copy OCR text", fn: () => ($("btn-ocr-copy") as HTMLButtonElement).click() },
  ]);
  $("btn-more").onclick = (e) => openMenu(e.currentTarget as HTMLElement, [
    {
      label: "Open image file…", fn: () => void (async () => {
        const f = await dialogOpen({ filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg"] }] }).catch(() => null);
        if (!f || Array.isArray(f)) return;
        try {
          const bytes = await readFile(f as string);
          let binary = ""; (bytes as Uint8Array).forEach((b: number) => (binary += String.fromCharCode(b)));
          await showImage(btoa(binary), "image/png", 0, 0, "file", false);
        } catch { toast("Unable to open file"); }
      })(),
    },
    { label: "Show in folder", fn: () => void (async () => { if (savedPath) await invoke("reveal_in_folder", { path: savedPath }).catch(() => toast("File not saved yet")); else toast("File not saved yet"); })() },
    { label: "History", fn: () => void invoke("ui_open_history") },
    { label: "Settings", fn: () => void invoke("ui_open_settings") },
    {
      label: "Resize…", fn: () => {
        $("edit-bar").classList.remove("hidden");
        $("resize-opts").classList.remove("hidden");
        ($("resize-w") as HTMLInputElement).value = String(baseW);
        ($("resize-h") as HTMLInputElement).value = String(baseH);
      },
    },
  ]);
  $("btn-cancel").onclick = () => void closeEditor(true);
  $("btn-crop-apply").onclick = () => void applyCrop();
  $("btn-crop-cancel").onclick = () => { cropRect = null; render(); };
  $("btn-resize-apply").onclick = () => void applyResize();

  $("zoom-in").onclick = () => { zoom = Math.min(4, zoom + 0.25); render(); };
  $("zoom-out").onclick = () => { zoom = Math.max(0.25, zoom - 0.25); render(); };
  $("zoom-fit").onclick = () => { fitZoom(); render(); };

  await listen<{ base64: string; mime?: string; width: number; height: number; source?: string; ocrMode?: boolean }>(
    "openscreen:show-image",
    async (ev) => {
      await showImage(ev.payload.base64, ev.payload.mime ?? "image/png", ev.payload.width, ev.payload.height, ev.payload.source ?? "screenshot", !!ev.payload.ocrMode);
    }
  );
  const win = getCurrentWindow();
  // Stay hidden at startup: only show when an image arrives (tray app, no main window).
  await win.hide().catch(() => {});
}

function fitZoom() {
  if (!baseImg) return;
  const r = wrap.getBoundingClientRect();
  zoom = Math.min(1.5, Math.min((r.width - 40) / baseW, (r.height - 40) / baseH));
  zoom = Math.max(0.25, zoom);
}

async function showImage(b64: string, mime: string, w: number, h: number, source: string, ocrMode: boolean) {
  // reset memory from previous image first
  currentBase64 = null; shapes = []; undoStack = []; redoStack = []; savedPath = null;
  currentBase64 = b64; currentMime = mime;
  ocrModeInitial = ocrMode;
  baseImg = await loadImage(base64ToDataUrl(b64, mime));
  baseW = baseImg.naturalWidth || w; baseH = baseImg.naturalHeight || h;
  $("st-source").textContent = source === "file" ? "Opened file" : source === "history" ? "History" : "Screenshot";
  $("ocr-text").textContent = "";
  setTool("select");
  fitZoom();
  render();
  await getCurrentWindow().show().catch(() => {});
  await getCurrentWindow().setFocus().catch(() => {});
  await autoBehaviors();
  if (ocrModeInitial && settings.ocrEnabled) { openOcrPanel(); void runOcr(); }
  void emit("openscreen:history-refresh", {});
}

async function applyCrop() {
  if (!cropRect || !baseImg) return;
  pushUndo();
  const { x, y, w, h } = cropRect;
  const off = document.createElement("canvas");
  off.width = Math.max(1, Math.floor(w)); off.height = Math.max(1, Math.floor(h));
  const octx = off.getContext("2d")!;
  // draw base + shapes then crop
  const full = document.createElement("canvas");
  full.width = baseW; full.height = baseH;
  const fctx = full.getContext("2d")!;
  fctx.drawImage(baseImg, 0, 0);
  octx.drawImage(full, x, y, w, h, 0, 0, off.width, off.height);
  currentBase64 = dataUrlToBase64(off.toDataURL("image/png"));
  baseImg = await loadImage(base64ToDataUrl(currentBase64));
  baseW = baseImg.naturalWidth; baseH = baseImg.naturalHeight;
  shapes = []; cropRect = null;
  render();
}

async function applyResize() {
  if (!baseImg) return;
  const w = parseInt(($("resize-w") as HTMLInputElement).value);
  const h = parseInt(($("resize-h") as HTMLInputElement).value);
  if (!w || !h || w < 1 || h < 1 || w > 8000 || h > 8000) { toast("Invalid size"); return; }
  pushUndo();
  const off = document.createElement("canvas");
  off.width = w; off.height = h;
  const octx = off.getContext("2d")!;
  octx.imageSmoothingQuality = "high";
  // flatten then scale for quality
  const flat = flattenedBase64("png");
  const img = await loadImage(base64ToDataUrl(flat));
  octx.drawImage(img, 0, 0, w, h);
  currentBase64 = dataUrlToBase64(off.toDataURL("image/png"));
  baseImg = await loadImage(base64ToDataUrl(currentBase64));
  baseW = w; baseH = h;
  shapes = [];
  $("resize-opts").classList.add("hidden");
  render();
}

async function closeEditor(discard: boolean) {
  void discard;
  // Free image memory on close
  currentBase64 = null; baseImg = null; shapes = [];
  undoStack = []; redoStack = [];
  ($("ocr-text") as HTMLTextAreaElement).value = "";
  await getCurrentWindow().hide().catch(() => {});
  // Keep settings fresh for next capture
  settings = await loadSettings().catch(() => settings);
}

void init();
