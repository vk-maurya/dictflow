import "./overlay.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalPosition } from "@tauri-apps/api/dpi";

type Phase = "idle" | "recording" | "transcribing";

interface OverlayPose {
  x: number;
  y: number;
  edge: "top" | "bottom" | "left" | "right";
  offset: number;
}

interface AppStatus {
  recording: boolean;
  transcribing: boolean;
}

interface LevelPayload {
  peak: number;
  rms?: number;
  bins?: number[];
  elapsed?: number;
}

const win = getCurrentWindow();
const BAR_COUNT = 40;
const BAR_W = 2.5;
const BAR_GAP = 2;
const NOISE_GATE = 0.02;
const PRESENCE_FULL = 0.08;
const MIN_BAR = 2;
const DRAG_PX = 12;

type Press = {
  sx: number;
  sy: number;
  originX: number;
  originY: number;
  ready: boolean;
};

let phase: Phase = "idle";
let bins = new Array<number>(BAR_COUNT).fill(0);
let press: Press | null = null;
let dragged = false;
let toggling = false;
let raf = 0;
let placing = false;
let pending: { x: number; y: number } | null = null;

const pill = () => document.getElementById("pill");
const canvas = () => document.getElementById("wave") as HTMLCanvasElement | null;
const timeEl = () => document.getElementById("time");

function formatTime(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  const m = Math.floor(s / 60);
  return `${m}:${(s % 60).toString().padStart(2, "0")}`;
}

function setTime(secs: number): void {
  const el = timeEl();
  if (el) el.textContent = formatTime(secs);
}

function setPhase(next: Phase): void {
  if (phase === next) return;
  phase = next;
  pill()?.classList.remove("idle", "recording", "transcribing", "copychip");
  pill()?.classList.add(next);
  if (next === "recording") {
    bins.fill(0);
    setTime(0);
    if (!raf) raf = requestAnimationFrame(tick);
    return;
  }
  if (raf) {
    cancelAnimationFrame(raf);
    raf = 0;
  }
  if (next === "idle") bins.fill(0);
}

function applyStatus(s: AppStatus): void {
  if (s.recording) setPhase("recording");
  else if (s.transcribing) setPhase("transcribing");
  else setPhase("idle");
}

function applyLevel(p: LevelPayload): void {
  if (phase !== "recording") return;
  if (Array.isArray(p.bins) && p.bins.length > 0) {
    const n = Math.min(BAR_COUNT, p.bins.length);
    for (let i = 0; i < BAR_COUNT; i++) {
      bins[i] = i < n ? Math.max(0, p.bins[i]) : 0;
    }
  } else {
    const v = Math.max(0, p.peak);
    bins.shift();
    bins.push(v);
  }
  if (typeof p.elapsed === "number") setTime(p.elapsed);
}

function roundBar(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
): void {
  const r = Math.min(w / 2, h / 2);
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
  ctx.fill();
}

function drawWave(): void {
  const el = canvas();
  if (!el || phase !== "recording") return;
  const dpr = window.devicePixelRatio || 1;
  const w = el.clientWidth;
  const h = el.clientHeight;
  if (w < 2 || h < 2) return;
  if (el.width !== Math.round(w * dpr) || el.height !== Math.round(h * dpr)) {
    el.width = Math.round(w * dpr);
    el.height = Math.round(h * dpr);
  }
  const ctx = el.getContext("2d");
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const gated = bins.map((s) => Math.max(0, s - NOISE_GATE));
  let peak = 0;
  for (const s of gated) peak = Math.max(peak, s);
  const recentPeak = Math.max(peak, 0.05);
  const presence = Math.min(1, peak / PRESENCE_FULL);

  const n = gated.length;
  const total = n * BAR_W + (n - 1) * BAR_GAP;
  let x = (w - total) / 2;
  ctx.fillStyle = "rgba(255, 255, 255, 0.9)";
  for (let i = 0; i < n; i++) {
    const norm = Math.min(1, gated[i] / recentPeak) * presence;
    const bh = Math.max(MIN_BAR, norm * h);
    roundBar(ctx, x, (h - bh) / 2, BAR_W, bh);
    x += BAR_W + BAR_GAP;
  }
}

function tick(): void {
  if (phase !== "recording") {
    raf = 0;
    return;
  }
  drawWave();
  raf = requestAnimationFrame(tick);
}

async function toggleFromOverlay(): Promise<void> {
  if (phase === "transcribing" || toggling) return;
  toggling = true;
  try {
    await invoke("toggle_recording");
  } catch {
    /* status poll reconciles */
  } finally {
    toggling = false;
  }
}

function resetPress(): void {
  press = null;
  dragged = false;
  pending = null;
}

async function captureOrigin(): Promise<void> {
  try {
    const pos = await win.outerPosition();
    const scale = await win.scaleFactor();
    if (!press) return;
    press.originX = pos.x / scale;
    press.originY = pos.y / scale;
    press.ready = true;
  } catch {
    /* origin is best-effort; drag is skipped until it lands */
  }
}

function queuePlace(x: number, y: number): void {
  pending = { x, y };
  if (placing) return;
  placing = true;
  void (async () => {
    while (pending && press) {
      const next = pending;
      pending = null;
      try {
        await win.setPosition(new LogicalPosition(next.x, next.y));
      } catch {
        break;
      }
    }
    placing = false;
  })();
}

function onPointerMove(e: PointerEvent): void {
  if (!press) return;
  const dx = e.screenX - press.sx;
  const dy = e.screenY - press.sy;
  if (!dragged) {
    if (Math.hypot(dx, dy) < DRAG_PX) return;
    dragged = true;
  }
  if (!press.ready) return;
  queuePlace(press.originX + dx, press.originY + dy);
}

async function snapAfterDrag(): Promise<void> {
  try {
    const pos = await win.outerPosition();
    const mon = await currentMonitor();
    if (!mon) return;
    const origin = mon.position;
    const size = mon.size;
    const scale = mon.scaleFactor || 1;
    const snapped = await invoke<OverlayPose>("snap_overlay", {
      x: Math.round((pos.x - origin.x) / scale),
      y: Math.round((pos.y - origin.y) / scale),
      screenW: Math.round(size.width / scale),
      screenH: Math.round(size.height / scale),
    });
    await win.setPosition(
      new LogicalPosition(origin.x / scale + snapped.x, origin.y / scale + snapped.y),
    );
    await invoke("set_overlay_pose", { edge: snapped.edge, offset: snapped.offset });
  } catch {
    /* pose is best-effort */
  }
}

async function boot(): Promise<void> {
  await win.setIgnoreCursorEvents(false).catch(() => undefined);
  document.addEventListener("contextmenu", (e) => e.preventDefault());
  setPhase("idle");

  const host = pill();
  if (host) {
    host.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      press = { sx: e.screenX, sy: e.screenY, originX: 0, originY: 0, ready: false };
      dragged = false;
      host.setPointerCapture(e.pointerId);
      void captureOrigin();
    });
    host.addEventListener("pointermove", onPointerMove);
    const onRelease = (e: PointerEvent) => {
      try {
        if (host.hasPointerCapture(e.pointerId)) {
          host.releasePointerCapture(e.pointerId);
        }
      } catch {
        /* capture already released */
      }
      const wasDrag = dragged;
      const hadPress = press !== null;
      resetPress();
      if (wasDrag) {
        void snapAfterDrag();
        return;
      }
      if (hadPress) void toggleFromOverlay();
    };
    host.addEventListener("pointerup", onRelease);
    host.addEventListener("pointercancel", onRelease);
  }

  await listen<boolean>("dictflow://recording", (e) => {
    if (e.payload) setPhase("recording");
    else if (phase === "recording") setPhase("transcribing");
  });
  await listen<boolean>("dictflow://transcribing", (e) => {
    if (e.payload) setPhase("transcribing");
    else setPhase("idle");
  });
  await listen<LevelPayload>("dictflow://level", (e) => applyLevel(e.payload));
  await listen("dictflow://committed", () => setPhase("idle"));
  await listen("dictflow://history-updated", () => {
    if (phase !== "recording") setPhase("idle");
  });

  setInterval(async () => {
    const s = await invoke<AppStatus>("get_status").catch(() => null);
    if (s) applyStatus(s);
  }, 500);

  const wave = canvas();
  if (wave && typeof ResizeObserver !== "undefined") {
    new ResizeObserver(() => {
      if (phase === "recording") drawWave();
    }).observe(wave);
  }
}

boot();
