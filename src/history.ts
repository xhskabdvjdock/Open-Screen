import { invoke } from "@tauri-apps/api/core";
import { listen, emit } from "@tauri-apps/api/event";
import { save as dialogSave } from "@tauri-apps/plugin-dialog";
import { icon } from "./icons";
import { toast, base64ToDataUrl } from "./common";

interface HistoryItem {
  id: string; createdAt: string; width: number; height: number;
  fileName: string; imagePath: string; thumbBase64: string;
}

async function load() {
  const list = document.getElementById("list")!;
  list.innerHTML = "<p class='muted'>Loading…</p>";
  let items: HistoryItem[] = [];
  try {
    items = await invoke<HistoryItem[]>("history_list");
  } catch { list.innerHTML = "<p class='muted'>History unavailable.</p>"; return; }
  if (!items.length) { list.innerHTML = "<p class='muted'>No screenshots yet.</p>"; return; }
  // Group by day
  const groups = new Map<string, HistoryItem[]>();
  for (const it of items) {
    const day = (it.createdAt || "").slice(0, 10) || "Older";
    if (!groups.has(day)) groups.set(day, []);
    groups.get(day)!.push(it);
  }
  list.innerHTML = "";
  for (const [day, arr] of groups) {
    const h = document.createElement("div");
    h.className = "day";
    h.textContent = day === new Date().toISOString().slice(0, 10) ? "Today" : day;
    list.appendChild(h);
    const grid = document.createElement("div");
    grid.id = "grid";
    for (const it of arr) {
      const card = document.createElement("div");
      card.className = "item card";
      const img = document.createElement("img");
      img.src = it.thumbBase64 ? base64ToDataUrl(it.thumbBase64) : "";
      img.alt = `${it.width}×${it.height}`;
      img.loading = "lazy";
      const meta = document.createElement("div");
      meta.className = "meta";
      const dims = document.createElement("span");
      dims.className = "muted";
      dims.textContent = `${it.width} × ${it.height} · ${(it.createdAt || "").slice(11, 16)}`;
      const actions = document.createElement("div");
      actions.className = "actions";
      const mk = (label: string, fn: () => void) => {
        const b = document.createElement("button");
        b.className = "btn"; b.textContent = label; b.style.padding = "4px 8px"; b.style.fontSize = "12px";
        b.onclick = fn; actions.appendChild(b);
      };
      mk("Copy", async () => {
        const b64 = await invoke<string>("history_get", { id: it.id });
        await invoke("clipboard_copy_image", { base64Png: b64 });
        toast("Copied");
      });
      mk("Open", async () => {
        const b64 = await invoke<string>("history_get", { id: it.id });
        await emit("openscreen:show-image", { base64: b64, mime: "image/png", width: it.width, height: it.height, source: "history" });
        await invoke("ui_open_editor").catch(() => {});
      });
      mk("Edit", async () => {
        const b64 = await invoke<string>("history_get", { id: it.id });
        await emit("openscreen:show-image", { base64: b64, mime: "image/png", width: it.width, height: it.height, source: "history" });
        await invoke("ui_open_editor").catch(() => {});
      });
      mk("OCR", async () => {
        const b64 = await invoke<string>("history_get", { id: it.id });
        await emit("openscreen:show-image", { base64: b64, mime: "image/png", width: it.width, height: it.height, source: "history", ocrMode: true });
        await invoke("ui_open_editor").catch(() => {});
      });
      mk("Save", async () => {
        const b64 = await invoke<string>("history_get", { id: it.id });
        const dest = await dialogSave({ defaultPath: it.fileName }).catch(() => null);
        if (!dest) return;
        await invoke("save_image_bytes", { base64Data: b64, path: dest });
        toast("Saved");
      });
      mk("Delete", async () => {
        await invoke("history_delete", { id: it.id });
        toast("Deleted");
        void load();
      });
      meta.append(dims, actions);
      card.append(img, meta);
      grid.appendChild(card);
    }
    list.appendChild(grid);
  }
}

async function init() {
  void icon;
  (document.getElementById("btn-refresh") as HTMLButtonElement).onclick = () => void load();
  (document.getElementById("btn-clear") as HTMLButtonElement).onclick = async () => {
    await invoke("history_clear").catch(() => {});
    toast("History cleared");
    void load();
  };
  (document.getElementById("btn-refresh-rec") as HTMLButtonElement).onclick = () => void loadRec();
  await listen("openscreen:history-refresh", () => { void load(); void loadRec(); });
  await load();
  await loadRec();
}

function fmtDur(s: number): string {
  const m = Math.floor(s / 60), r = Math.floor(s % 60);
  return `${String(m).padStart(2, "0")}:${String(r).padStart(2, "0")}`;
}
function fmtSize(b: number): string {
  if (b > 1073741824) return `${(b / 1073741824).toFixed(2)} GB`;
  if (b > 1048576) return `${(b / 1048576).toFixed(1)} MB`;
  if (b > 1024) return `${(b / 1024).toFixed(0)} KB`;
  return `${b} B`;
}

interface RecItem {
  id: string; name: string; path: string; createdAt: string;
  durationS: number; width: number; height: number; fps: number; size: number; kind: string;
}

async function loadRec() {
  const box = document.getElementById("rec-list")!;
  box.innerHTML = "<p class='muted'>Loading…</p>";
  let items: RecItem[] = [];
  try {
    items = await invoke<RecItem[]>("rec_history_list");
  } catch { box.innerHTML = "<p class='muted'>Recordings unavailable.</p>"; return; }
  if (!items.length) { box.innerHTML = "<p class='muted'>No recordings yet. Press Ctrl+Shift+R or use the tray menu.</p>"; return; }
  box.innerHTML = "";
  const grid = document.createElement("div");
  grid.id = "grid";
  for (const it of items) {
    const card = document.createElement("div");
    card.className = "item card";
    const meta = document.createElement("div");
    meta.className = "meta";
    const title = document.createElement("span");
    title.textContent = (it.kind === "replay" ? "Replay · " : "") + it.name;
    title.style.fontWeight = "600";
    title.style.wordBreak = "break-all";
    const dims = document.createElement("span");
    dims.className = "muted";
    dims.textContent = `${fmtDur(it.durationS)} · ${it.width}×${it.height}@${it.fps} · ${fmtSize(it.size)} · ${(it.createdAt || "").slice(0, 16).replace("T", " ")}`;
    const actions = document.createElement("div");
    actions.className = "actions";
    const mk = (label: string, fn: () => void) => {
      const b = document.createElement("button");
      b.className = "btn"; b.textContent = label; b.style.padding = "4px 8px"; b.style.fontSize = "12px";
      b.onclick = fn; actions.appendChild(b);
    };
    mk("Open", () => void invoke("open_path", { path: it.path }).catch(() => toast("File missing")));
    mk("Folder", () => void invoke("reveal_in_folder", { path: it.path }).catch(() => toast("File missing")));
    mk("Copy Path", async () => {
      await invoke("clipboard_copy_text", { text: it.path }).catch(() => {});
      toast("Path copied");
    });
    mk("Delete", async () => {
      await invoke("rec_history_delete", { id: it.id });
      toast("Deleted");
      void loadRec();
    });
    meta.append(title, dims, actions);
    card.appendChild(meta);
    grid.appendChild(card);
  }
  box.appendChild(grid);
}
void init();
