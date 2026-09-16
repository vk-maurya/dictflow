//! Overlay pill geometry + copy-chip visibility. No window handles.

use serde::{Deserialize, Serialize};

pub const PILL_W: i32 = 168;
pub const PILL_H: i32 = 44;
pub const MARGIN: i32 = 16;
/// Logical pixels above the screen bottom so the pill clears the Windows
/// taskbar (~40) and the macOS Dock (~68–80) on Retina displays.
pub const BOTTOM_MARGIN: i32 = 108;
pub const COPY_CHIP_SECS: u64 = 10;
pub const COPY_CHIP_MS: u64 = COPY_CHIP_SECS * 1000;
/// Offset `< 0` (or `0` on a fresh install) means center along the docked edge.
pub const OFFSET_CENTER: i32 = -1;

pub fn centered_offset(screen: i32, pill: i32) -> i32 {
    ((screen - pill) / 2).max(MARGIN)
}

fn along_axis(offset: i32, screen: i32, pill: i32) -> i32 {
    let max = (screen - pill - MARGIN).max(MARGIN);
    if offset <= 0 {
        clamp(centered_offset(screen, pill), MARGIN, max)
    } else {
        clamp(offset, MARGIN, max)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl DockEdge {
    pub fn as_str(self) -> &'static str {
        match self {
            DockEdge::Top => "top",
            DockEdge::Bottom => "bottom",
            DockEdge::Left => "left",
            DockEdge::Right => "right",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "top" => Some(DockEdge::Top),
            "bottom" => Some(DockEdge::Bottom),
            "left" => Some(DockEdge::Left),
            "right" => Some(DockEdge::Right),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayPose {
    pub x: i32,
    pub y: i32,
    pub edge: DockEdge,
    pub offset: i32,
}

fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi)
}

fn max_bottom_y(screen_h: i32, pill_h: i32) -> i32 {
    (screen_h - pill_h - BOTTOM_MARGIN).max(MARGIN)
}

/// Snap a dragged origin to the nearest screen edge.
pub fn snap_to_edge(
    x: i32,
    y: i32,
    screen_w: i32,
    screen_h: i32,
    pill_w: i32,
    pill_h: i32,
) -> OverlayPose {
    let max_x = (screen_w - pill_w - MARGIN).max(MARGIN);
    let max_y = max_bottom_y(screen_h, pill_h);
    let cx = x + pill_w / 2;
    let cy = y + pill_h / 2;
    let d_top = cy;
    let d_bottom = screen_h - cy;
    let d_left = cx;
    let d_right = screen_w - cx;
    let nearest = d_top
        .min(d_bottom)
        .min(d_left)
        .min(d_right);
    if nearest == d_top {
        let ox = clamp(x, MARGIN, max_x);
        OverlayPose {
            x: ox,
            y: MARGIN,
            edge: DockEdge::Top,
            offset: ox,
        }
    } else if nearest == d_bottom {
        let ox = clamp(x, MARGIN, max_x);
        OverlayPose {
            x: ox,
            y: max_y,
            edge: DockEdge::Bottom,
            offset: ox,
        }
    } else if nearest == d_left {
        let oy = clamp(y, MARGIN, max_y);
        OverlayPose {
            x: MARGIN,
            y: oy,
            edge: DockEdge::Left,
            offset: oy,
        }
    } else {
        let oy = clamp(y, MARGIN, max_y);
        OverlayPose {
            x: max_x,
            y: oy,
            edge: DockEdge::Right,
            offset: oy,
        }
    }
}

pub fn pose_to_xy(
    edge: DockEdge,
    offset: i32,
    screen_w: i32,
    screen_h: i32,
    pill_w: i32,
    pill_h: i32,
) -> (i32, i32) {
    let max_x = (screen_w - pill_w - MARGIN).max(MARGIN);
    let max_y = max_bottom_y(screen_h, pill_h);
    match edge {
        DockEdge::Top => (along_axis(offset, screen_w, pill_w), MARGIN),
        DockEdge::Bottom => (along_axis(offset, screen_w, pill_w), max_y),
        DockEdge::Left => (MARGIN, along_axis(offset, screen_h, pill_h)),
        DockEdge::Right => (max_x, along_axis(offset, screen_h, pill_h)),
    }
}

pub fn copy_chip_visible(pasted_unix_ms: u64, now_unix_ms: u64, window_ms: u64) -> bool {
    now_unix_ms >= pasted_unix_ms && now_unix_ms - pasted_unix_ms < window_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    const SW: i32 = 1920;
    const SH: i32 = 1080;

    #[test]
    fn snap_from_top_center() {
        let p = snap_to_edge(800, 40, SW, SH, PILL_W, PILL_H);
        assert_eq!(p.edge, DockEdge::Top);
        assert_eq!(p.y, MARGIN);
        assert_eq!(p.x, 800);
    }

    #[test]
    fn snap_from_left() {
        let p = snap_to_edge(20, 400, SW, SH, PILL_W, PILL_H);
        assert_eq!(p.edge, DockEdge::Left);
        assert_eq!(p.x, MARGIN);
    }

    #[test]
    fn clamp_past_corner() {
        let p = snap_to_edge(3000, 10, SW, SH, PILL_W, PILL_H);
        assert_eq!(p.edge, DockEdge::Right);
        assert_eq!(p.x, SW - PILL_W - MARGIN);
        assert!(p.y >= MARGIN && p.y <= SH - PILL_H - MARGIN);
    }

    #[test]
    fn pose_round_trip() {
        let snapped = snap_to_edge(100, 20, SW, SH, PILL_W, PILL_H);
        let (x, y) = pose_to_xy(
            snapped.edge,
            snapped.offset,
            SW,
            SH,
            PILL_W,
            PILL_H,
        );
        assert_eq!((x, y), (snapped.x, snapped.y));
    }

    #[test]
    fn copy_chip_window() {
        assert!(copy_chip_visible(0, 9_999, COPY_CHIP_MS));
        assert!(!copy_chip_visible(0, 10_001, COPY_CHIP_MS));
        assert!(!copy_chip_visible(100, 50, COPY_CHIP_MS));
    }

    #[test]
    fn edge_parse() {
        assert_eq!(DockEdge::parse("top"), Some(DockEdge::Top));
        assert_eq!(DockEdge::parse("nope"), None);
        assert_eq!(DockEdge::Right.as_str(), "right");
    }

    #[test]
    fn zero_offset_centers_on_bottom() {
        let (x, y) = pose_to_xy(DockEdge::Bottom, 0, SW, SH, PILL_W, PILL_H);
        assert_eq!(x, centered_offset(SW, PILL_W));
        assert_eq!(y, SH - PILL_H - BOTTOM_MARGIN);
        assert!(y + PILL_H + 80 <= SH, "must clear a typical taskbar or Dock");
        let (cx, _) = pose_to_xy(DockEdge::Bottom, OFFSET_CENTER, SW, SH, PILL_W, PILL_H);
        assert_eq!(cx, x);
    }
}
