//! Live tray icon: a tiny CPU history graph (newest sample on the right,
//! colour by load) plus the tooltip text with CPU / RAM / GPU usage and
//! temperatures. Pure drawing here; the window code swaps it into the tray.

use crate::sample::Snapshot;
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

const BG: u32 = 0xff0d1117;
const BORDER: u32 = 0xff343c48;
const COOL: u32 = 0xff3fd0e0; // same cyan as the CPU colour in the app
const WARM: u32 = 0xffffb347;
const HOT: u32 = 0xffe5484d;

fn lighten(c: u32) -> u32 {
    let ch = |s: u32| (((c >> s) & 0xff) + 60).min(255) << s;
    0xff00_0000 | ch(16) | ch(8) | ch(0)
}

fn load_color(v: f32) -> u32 {
    if v >= 90.0 {
        HOT
    } else if v >= 70.0 {
        WARM
    } else {
        COOL
    }
}

/// `size`×`size` ARGB pixels (0xAARRGGBB): bordered box, one column per
/// recent CPU sample, newest at the right. A 100 % sample fills its column.
pub fn render(hist: &[f32], size: usize) -> Vec<u32> {
    let mut px = vec![BG; size * size];
    for i in 0..size {
        px[i] = BORDER;
        px[(size - 1) * size + i] = BORDER;
        px[i * size] = BORDER;
        px[i * size + size - 1] = BORDER;
    }
    let inner = size.saturating_sub(2);
    if inner == 0 {
        return px;
    }
    let take = hist.len().min(inner);
    let recent = &hist[hist.len() - take..];
    for (k, v) in recent.iter().enumerate() {
        let col = 1 + inner - take + k;
        let h = ((v.clamp(0.0, 100.0) / 100.0) * inner as f32).round() as usize;
        let h = if *v > 0.5 { h.max(1) } else { 0 };
        let c = load_color(*v);
        for j in 0..h {
            let y = size - 2 - j;
            px[y * size + col] = if j + 1 == h { lighten(c) } else { c };
        }
    }
    px
}

/// Build an HICON from the rendered pixels. The caller destroys it.
pub fn make_icon(hist: &[f32], size: usize) -> Option<HICON> {
    let px = render(hist, size);
    let mask = vec![0u8; size * size / 8 + size]; // all opaque; alpha lives in the colour bitmap
    unsafe {
        let color = CreateBitmap(size as i32, size as i32, 1, 32, Some(px.as_ptr() as *const _));
        let m = CreateBitmap(size as i32, size as i32, 1, 1, Some(mask.as_ptr() as *const _));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: m, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(m.0));
        icon
    }
}

fn temp(t: Option<f32>) -> String {
    t.map(|t| format!(" · {t:.0}°C")).unwrap_or_default()
}

/// Tooltip lines (the shell shows up to 127 characters).
pub fn tooltip(s: &Snapshot) -> String {
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let used = s.mem.total.saturating_sub(s.mem.avail);
    let pct = if s.mem.total > 0 { used * 100 / s.mem.total } else { 0 };
    let mut t = format!("CPU {:.0}%{}\nRAM {:.1} / {:.0} GB ({pct}%)", s.cpu, temp(s.cpu_temp), gb(used), gb(s.mem.total));
    if let Some(g) = s.gpus.first().filter(|g| g.max > 0.0 && g.unit == "%") {
        t += &format!("\nGPU {:.0}%{}", g.value, temp(g.temp));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_load_fills_the_column() {
        let px = render(&[100.0], 16);
        // newest sample sits in the last inner column (x = 14), full height
        for y in 1..15 {
            assert_ne!(px[y * 16 + 14], BG, "y={y}");
        }
        assert_eq!(px[0], BORDER);
    }

    #[test]
    fn idle_draws_nothing_inside() {
        let px = render(&[0.0; 14], 16);
        for y in 1..15 {
            for x in 1..15 {
                assert_eq!(px[y * 16 + x], BG);
            }
        }
    }

    #[test]
    fn small_load_still_shows_one_pixel_and_colours_by_level() {
        let px = render(&[1.0, 75.0, 95.0], 16);
        assert_ne!(px[14 * 16 + 12], BG); // 1 % -> one pixel
        let top = (1..15).find(|y| px[y * 16 + 13] != BG).unwrap();
        assert_eq!(px[top * 16 + 13], lighten(WARM)); // 75 % column: warm colour, bright cap
        let top95 = (1..15).find(|y| px[y * 16 + 14] != BG).unwrap();
        assert_eq!(px[top95 * 16 + 14], lighten(HOT)); // 95 % column: red, bright cap
    }

    #[test]
    fn long_history_is_cut_to_the_icon_width() {
        let h: Vec<f32> = (0..200).map(|i| (i % 100) as f32).collect();
        assert_eq!(render(&h, 32).len(), 32 * 32);
    }

    #[test]
    fn tooltip_lists_cpu_ram_gpu() {
        let mut s = Snapshot::default();
        s.cpu = 12.4;
        s.cpu_temp = Some(59.0);
        s.mem.total = 64 << 30;
        s.mem.avail = 27 << 30;
        s.gpus.push(crate::sample::Part { title: "GPU".into(), value: 41.0, unit: "%", max: 100.0, sub: String::new(), temp: Some(34.0), temp_stale: false, hist: vec![] });
        let t = tooltip(&s);
        assert!(t.contains("CPU 12% · 59°C"), "{t}");
        assert!(t.contains("RAM 37.0 / 64 GB (57%)"), "{t}");
        assert!(t.contains("GPU 41% · 34°C"), "{t}");
        assert!(t.encode_utf16().count() < 127);
    }
}
