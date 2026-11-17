import "./overlay.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Phase = "idle" | "recording" | "transcribing" | "refining";

interface AppStatus {
  recording: boolean;
  transcribing: boolean;
  refining?: boolean;
}

interface LevelPayload {
  peak: number;
  rms?: number;
  bins?: number[];
  elapsed?: number;
}

const BAR_COUNT = 40;
const BAR_W = 2.5;
const BAR_GAP = 2;
const NOISE_GATE = 0.02;
const PRESENCE_FULL = 0.08;
const MIN_BAR = 2;
const DRAG_PX = 12;
/// Must match `src/overlay.css` phase footprints. The pill is centered in the
/// overlay window, so the backend gate knows where the pill sits on screen.
const HIT_SIZE: Record<Phase, { w: number; h: number }> = {
  idle: { w: 48, h: 11 },
  recording: { w: 146, h: 20 },
  transcribing: { w: 148, h: 20 },
  refining: { w: 148, h: 20 },
};

type Press = {
  sx: number;
  sy: number;
};

let phase: Phase = "idle";
let bins = new Array<number>(BAR_COUNT).fill(0);
let press: Press | null = null;
let dragged = false;
let toggling = false;
let raf = 0;
let placing = false;
let pending: { dx: number; dy: number } | null = null;

const pill = () => document.getElementById("pill");
const canvas = () => document.getElementById("wave") as HTMLCanvasElement | null;

function setPhase(next: Phase): void {
  if (phase === next) return;
  phase = next;
  pill()?.classList.remove("idle", "recording", "transcribing", "refining", "copychip");
  pill()?.classList.add(next);
  reportHitRect();
  const status = document.getElementById("status");
  if (status) {
    if (next === "refining") status.textContent = "Refining...";
    else if (next === "transcribing") status.textContent = "Transcribing...";
  }
  if (next === "recording") {
    bins.fill(0);
    if (!raf) raf = requestAnimationFrame(tick);
    return;
  }
  if (raf) {
    cancelAnimationFrame(raf);
    raf = 0;
  }
  if (next === "idle") bins.fill(0);
}

/// Tell the backend exactly which window-relative rect the pill covers once
/// its size transition settles, so hover-gated click-through matches the
/// visible pill. The target footprint is known statically; a measured
/// confirmation lands after the morph transition in case CSS drifted.
function reportHitRect(): void {
  const el = pill();
  if (!el) return;
  const target: Phase = phase;
  const { w, h } = HIT_SIZE[target];
  void invoke("set_overlay_hit_rect", {
    x: Math.round((document.body.clientWidth - w) / 2),
    y: Math.round((document.body.clientHeight - h) / 2),
    w,
    h,
  }).catch(() => undefined);
  window.setTimeout(() => {
    if (phase !== target) return; // a newer morph already reported
    const r = el.getBoundingClientRect();
    void invoke("set_overlay_hit_rect", {
      x: Math.round(r.left),
      y: Math.round(r.top),
      w: Math.round(r.width),
      h: Math.round(r.height),
    }).catch(() => undefined);
  }, 340);
}

function applyStatus(s: AppStatus): void {
  if (s.recording) setPhase("recording");
  else if (s.transcribing) setPhase("transcribing");
  else if (s.refining) setPhase("refining");
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
  const natural = n * BAR_W + (n - 1) * BAR_GAP;
  const vertical = pill()?.classList.contains("vertical") ?? false;
  const room = vertical ? h : w;
  const fit = natural > room && natural > 0 ? room / natural : 1;
  const bar = BAR_W * fit;
  const gap = BAR_GAP * fit;
  const total = n * bar + (n - 1) * gap;
  ctx.fillStyle = "rgba(232, 227, 214, 0.92)";
  if (vertical) {
    let y = (h - total) / 2;
    for (let i = 0; i < n; i++) {
      const norm = Math.min(1, gated[i] / recentPeak) * presence;
      const bw = Math.max(MIN_BAR, norm * w);
      roundBar(ctx, (w - bw) / 2, y, bw, bar);
      y += bar + gap;
    }
    return;
  }
  let x = (w - total) / 2;
  for (let i = 0; i < n; i++) {
    const norm = Math.min(1, gated[i] / recentPeak) * presence;
    const bh = Math.max(MIN_BAR, norm * h);
    roundBar(ctx, x, (h - bh) / 2, bar, bh);
    x += bar + gap;
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
  if (phase === "transcribing" || phase === "refining" || toggling) return;
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

function queueMove(dx: number, dy: number): void {
  pending = { dx, dy };
  if (placing) return;
  placing = true;
  void (async () => {
    while (pending && press) {
      const next = pending;
      pending = null;
      try {
        await invoke("move_overlay_by", { dx: next.dx, dy: next.dy });
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
  queueMove(dx, dy);
}

function applyEdge(edge: string): void {
  pill()?.classList.toggle("vertical", edge === "left" || edge === "right");
}

async function boot(): Promise<void> {
  // Clickability is owned by the backend hover gate (`hover.rs`): the whole
  // window is click-through except the pill's reported hit rect. Do not
  // force it clickable here.
  document.addEventListener("contextmenu", (e) => e.preventDefault());
  setPhase("idle");
  reportHitRect(); // boot's setPhase early-returns (already idle)

  const host = pill();
  if (host) {
    host.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      press = { sx: e.screenX, sy: e.screenY };
      dragged = false;
      void invoke("begin_overlay_move");
      host.setPointerCapture(e.pointerId);
      // Keep the backend hover gate from flipping to pass-through mid-drag.
      void invoke("set_overlay_drag", { active: true }).catch(() => undefined);
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
      void (async () => {
        await invoke("end_overlay_move", { moved: wasDrag }).catch(() => undefined);
        await invoke("set_overlay_drag", { active: false }).catch(() => undefined);
        if (!wasDrag && hadPress) await toggleFromOverlay();
      })();
    };
    host.addEventListener("pointerup", onRelease);
    host.addEventListener("pointercancel", onRelease);
  }

  await listen<string>("dictflow://overlay-edge", (e) => applyEdge(e.payload));
  const saved = await invoke<{ overlay_edge?: string }>("get_settings").catch(() => null);
  if (saved?.overlay_edge) applyEdge(saved.overlay_edge);

  // The pointer is tracked natively: a window that is not active gets no
  // hover events from the webview, so Rust says when the pointer is on the pill.
  await listen<boolean>("dictflow://overlay-hover", (e) => {
    pill()?.classList.toggle("hovering", e.payload);
  });

  await listen<boolean>("dictflow://recording", (e) => {
    if (e.payload) setPhase("recording");
    else if (phase === "recording") setPhase("transcribing");
  });
  await listen<boolean>("dictflow://transcribing", (e) => {
    if (e.payload) setPhase("transcribing");
    else if (phase !== "refining") setPhase("idle");
  });
  await listen<boolean>("dictflow://refining", (e) => {
    if (e.payload) setPhase("refining");
    else if (phase === "refining") setPhase("idle");
  });
  await listen<LevelPayload>("dictflow://level", (e) => applyLevel(e.payload));
  await listen("dictflow://committed", () => {
    if (phase !== "refining") setPhase("idle");
  });
  await listen("dictflow://history-updated", () => {
    if (phase !== "recording" && phase !== "refining") setPhase("idle");
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
