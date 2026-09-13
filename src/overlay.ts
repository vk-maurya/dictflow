import "./overlay.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";
import { PhysicalPosition } from "@tauri-apps/api/dpi";

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

const win = getCurrentWindow();
const HISTORY = 40;
const DRAG_PX = 12;

let phase: Phase = "idle";
let peak = 0;
let press: { x: number; y: number } | null = null;
let dragged = false;
let toggling = false;
let raf = 0;
const history = new Array<number>(HISTORY).fill(0.04);

const pill = () => document.getElementById("pill");
const canvas = () => document.getElementById("wave") as HTMLCanvasElement | null;

function setPhase(next: Phase): void {
  if (phase === next) return;
  phase = next;
  pill()?.classList.remove("idle", "recording", "transcribing", "copychip");
  pill()?.classList.add(next);
  if (next !== "recording") peak = 0;
  if (next === "idle") {
    history.fill(0.04);
    if (raf) {
      cancelAnimationFrame(raf);
      raf = 0;
    }
    requestAnimationFrame(() => drawWave(0));
    return;
  }
  if (!raf) raf = requestAnimationFrame(tick);
}

function applyStatus(s: AppStatus): void {
  if (s.recording) setPhase("recording");
  else if (s.transcribing) setPhase("transcribing");
  else setPhase("idle");
}

function pushLevel(v: number): void {
  history.shift();
  history.push(Math.max(0.03, Math.min(1, v)));
}

function drawWave(t: number): void {
  const el = canvas();
  if (!el) return;
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
  ctx.beginPath();
  ctx.lineWidth = 1.6;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  ctx.strokeStyle = phase === "recording" ? "#f5b942" : "#e8e4da";

  const mid = h / 2;
  if (phase === "idle") {
    ctx.moveTo(0, mid);
    ctx.lineTo(w, mid);
    ctx.stroke();
    return;
  }

  for (let i = 0; i < HISTORY; i++) {
    const x = (i / (HISTORY - 1)) * w;
    const amp =
      phase === "recording"
        ? 0.18 + history[i] * 0.72
        : 0.28 + Math.sin(t * 0.006 + i * 0.35) * 0.12;
    const wobble = Math.sin(t * 0.004 + i * 0.5) * 0.08;
    const y = mid + Math.sin((i / HISTORY) * Math.PI * 3.2 + t * 0.003) * (amp + wobble) * mid;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

function tick(now: number): void {
  if (phase === "idle") {
    raf = 0;
    drawWave(0);
    return;
  }
  if (phase === "recording") pushLevel(0.12 + Math.min(peak * 3.2, 0.88));
  else pushLevel(0.3);
  drawWave(now);
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
}

async function snapAfterDrag(): Promise<void> {
  try {
    const pos = await win.outerPosition();
    const mon = await currentMonitor();
    if (!mon) return;
    const origin = mon.position;
    const size = mon.size;
    const snapped = await invoke<OverlayPose>("snap_overlay", {
      x: pos.x - origin.x,
      y: pos.y - origin.y,
      screenW: size.width,
      screenH: size.height,
    });
    await win.setPosition(new PhysicalPosition(origin.x + snapped.x, origin.y + snapped.y));
    await invoke("set_overlay_pose", { edge: snapped.edge, offset: snapped.offset });
  } catch {
    /* pose is best-effort */
  }
}

async function boot(): Promise<void> {
  await win.setIgnoreCursorEvents(false).catch(() => undefined);
  document.addEventListener("contextmenu", (e) => e.preventDefault());
  setPhase("idle");
  document.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    press = { x: e.clientX, y: e.clientY };
    dragged = false;
  });
  document.addEventListener("pointermove", (e) => {
    if (!press || dragged) return;
    if (Math.hypot(e.clientX - press.x, e.clientY - press.y) < DRAG_PX) return;
    dragged = true;
    win.startDragging().catch(() => undefined);
  });
  const onRelease = () => {
    const wasDrag = dragged;
    const hadPress = press !== null;
    resetPress();
    if (wasDrag) {
      snapAfterDrag();
      return;
    }
    if (hadPress) toggleFromOverlay();
  };
  window.addEventListener("pointerup", onRelease);
  window.addEventListener("pointercancel", resetPress);

  await listen<boolean>("dictflow://recording", (e) => {
    if (e.payload) setPhase("recording");
    else if (phase === "recording") setPhase("transcribing");
  });
  await listen<boolean>("dictflow://transcribing", (e) => {
    if (e.payload) setPhase("transcribing");
    else setPhase("idle");
  });
  await listen<{ peak: number }>("dictflow://level", (e) => {
    if (phase === "recording") peak = e.payload.peak;
  });
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
      if (phase === "idle") drawWave(0);
    }).observe(wave);
  }
  requestAnimationFrame(() => drawWave(0));
}

boot();
