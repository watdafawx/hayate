//! User UI zoom: one global scale on top of the system DPI.
//!
//! The Direct2D render target runs at `system dpi * zoom`, so every view lays
//! out in the same logical units as before (the logical client size and the
//! mouse coordinates are divided by the same factor). Nothing per-view knows
//! about zoom. Persisted in its own tiny `zoom.ini` (`zoom=125`) in the data
//! dir; written only when the user changes the zoom, never at start-up (so an
//! elevated cold start never writes the medium-writable data dir).

use std::sync::atomic::{AtomicU32, Ordering};

/// Allowed zoom percentages, ascending.
pub const STEPS: [u32; 10] = [80, 90, 100, 110, 125, 150, 175, 200, 250, 300];
pub const DEFAULT: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    In,
    Out,
    Reset,
}

static CUR: AtomicU32 = AtomicU32::new(DEFAULT);

/// Current zoom in percent (always one of `STEPS`).
pub fn percent() -> u32 {
    CUR.load(Ordering::Relaxed)
}

/// Current zoom as a factor (1.0 = 100%).
pub fn factor() -> f32 {
    percent() as f32 / 100.0
}

pub fn set(p: u32) {
    CUR.store(snap(p), Ordering::Relaxed);
}

/// Nearest step to `p` (ties go up); out-of-range values clamp.
pub fn snap(p: u32) -> u32 {
    let mut best = STEPS[0];
    for s in STEPS {
        if p.abs_diff(s) < p.abs_diff(best) || (p.abs_diff(s) == p.abs_diff(best) && s > best) {
            best = s;
        }
    }
    best
}

/// The next larger step (stays at the maximum).
pub fn next(p: u32) -> u32 {
    let p = snap(p);
    STEPS.iter().copied().find(|&s| s > p).unwrap_or(p)
}

/// The next smaller step (stays at the minimum).
pub fn prev(p: u32) -> u32 {
    let p = snap(p);
    STEPS.iter().copied().rev().find(|&s| s < p).unwrap_or(p)
}

/// The zoom after `cmd` from `p`.
pub fn apply(cmd: Cmd, p: u32) -> u32 {
    match cmd {
        Cmd::In => next(p),
        Cmd::Out => prev(p),
        Cmd::Reset => DEFAULT,
    }
}

/// Key combo -> command. Ctrl+= / Ctrl+Plus (main and numpad), Ctrl+-, Ctrl+0.
/// Shift is accepted (Ctrl+Plus is Ctrl+Shift+= on a US layout).
pub fn key_cmd(vk: u16, ctrl: bool) -> Option<Cmd> {
    if !ctrl {
        return None;
    }
    match vk {
        0xBB | 0x6B => Some(Cmd::In),   // VK_OEM_PLUS, VK_ADD
        0xBD | 0x6D => Some(Cmd::Out),  // VK_OEM_MINUS, VK_SUBTRACT
        0x30 | 0x60 => Some(Cmd::Reset), // '0', VK_NUMPAD0
        _ => None,
    }
}

/// Ctrl + wheel: forward (positive delta) zooms in.
pub fn wheel_cmd(delta: f32) -> Option<Cmd> {
    if delta > 0.0 {
        Some(Cmd::In)
    } else if delta < 0.0 {
        Some(Cmd::Out)
    } else {
        None
    }
}

/// Device pixels -> logical units at `scale` (system dpi / 96 * zoom).
pub fn to_logical(px: f32, scale: f32) -> f32 {
    px / scale.max(0.01)
}

/// Logical units -> device pixels.
pub fn to_device(v: f32, scale: f32) -> f32 {
    v * scale.max(0.01)
}

/// Text of the `zoom.ini` file.
pub fn format_ini(p: u32) -> String {
    format!("zoom={}\n", snap(p))
}

/// The zoom a `zoom.ini` holds; None for garbage or a missing key. A number
/// off the step table snaps to the nearest step.
pub fn parse_ini(text: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if let Some(v) = line.strip_prefix("zoom") {
            let v = v.trim_start().strip_prefix('=')?.trim().trim_end_matches('%').trim();
            let n: u32 = v.parse().ok()?;
            if n == 0 || n > 10_000 {
                return None;
            }
            return Some(snap(n));
        }
    }
    None
}

fn path() -> std::path::PathBuf {
    crate::osutil::data_dir().join("zoom.ini")
}

/// Read the saved zoom into the global (default 100 when absent or bad).
pub fn load() {
    let p = std::fs::read_to_string(path()).ok().and_then(|t| parse_ini(&t)).unwrap_or(DEFAULT);
    set(p);
}

/// Save the current zoom. Called only after a user change.
pub fn save() {
    if crate::osutil::cold_started() {
        return;
    }
    let _ = std::fs::write(path(), format_ini(percent()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_next_prev_clamp() {
        assert_eq!(next(100), 110);
        assert_eq!(prev(100), 90);
        assert_eq!(next(300), 300);
        assert_eq!(prev(80), 80);
        assert_eq!(next(5), 90);
        assert_eq!(prev(9999), 250);
        assert_eq!(next(103), 110);
        assert_eq!(apply(Cmd::Reset, 250), 100);
        assert_eq!(apply(Cmd::In, 250), 300);
        assert_eq!(apply(Cmd::Out, 100), 90);
        let mut p = 100;
        for _ in 0..20 {
            p = apply(Cmd::In, p);
        }
        assert_eq!(p, 300);
        for _ in 0..20 {
            p = apply(Cmd::Out, p);
        }
        assert_eq!(p, 80);
    }

    #[test]
    fn snapping() {
        assert_eq!(snap(0), 80);
        assert_eq!(snap(100), 100);
        assert_eq!(snap(116), 110);
        assert_eq!(snap(118), 125);
        assert_eq!(snap(120), 125);
        assert_eq!(snap(1000), 300);
    }

    #[test]
    fn ini_roundtrip_and_garbage() {
        assert_eq!(format_ini(150), "zoom=150\n");
        assert_eq!(parse_ini(&format_ini(175)), Some(175));
        assert_eq!(parse_ini("zoom = 200"), Some(200));
        assert_eq!(parse_ini("# c\r\nzoom=125%\r\n"), Some(125));
        assert_eq!(parse_ini("\u{feff}zoom=90"), Some(90));
        assert_eq!(parse_ini("zoom=133"), Some(125));
        assert_eq!(parse_ini(""), None);
        assert_eq!(parse_ini("zoom="), None);
        assert_eq!(parse_ini("zoom=abc"), None);
        assert_eq!(parse_ini("zoom=-5"), None);
        assert_eq!(parse_ini("zoom=0"), None);
        assert_eq!(parse_ini("zoom=99999999999"), None);
        assert_eq!(parse_ini("size=100"), None);
        assert_eq!(parse_ini("\0\0\u{1}garbage"), None);
    }

    #[test]
    fn key_combos() {
        assert_eq!(key_cmd(0xBB, true), Some(Cmd::In));
        assert_eq!(key_cmd(0x6B, true), Some(Cmd::In));
        assert_eq!(key_cmd(0xBD, true), Some(Cmd::Out));
        assert_eq!(key_cmd(0x6D, true), Some(Cmd::Out));
        assert_eq!(key_cmd(0x30, true), Some(Cmd::Reset));
        assert_eq!(key_cmd(0x60, true), Some(Cmd::Reset));
        assert_eq!(key_cmd(0xBB, false), None);
        assert_eq!(key_cmd(0x30, false), None);
        assert_eq!(key_cmd(b'F' as u16, true), None);
    }

    #[test]
    fn wheel_and_coords() {
        assert_eq!(wheel_cmd(120.0), Some(Cmd::In));
        assert_eq!(wheel_cmd(-120.0), Some(Cmd::Out));
        assert_eq!(wheel_cmd(0.0), None);
        assert_eq!(to_logical(300.0, 3.0), 100.0);
        assert_eq!(to_device(100.0, 2.5), 250.0);
        assert!((to_logical(to_device(37.0, 1.75), 1.75) - 37.0).abs() < 1e-4);
        assert!(to_logical(10.0, 0.0).is_finite());
    }
}
