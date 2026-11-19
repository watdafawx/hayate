//! App icons wherever a process, service or app is named (Processes,
//! Vitals, Services, Startup, Rules, Watch).
//!
//! Paint never touches the shell: it looks the image path up in a small
//! cache and, on a miss, queues the path for the "icons" worker thread and
//! draws the placeholder square. The worker reads the icon straight from the
//! exe's resources at the exact pixel size (SHDefExtractIconW: no system
//! image list, no shell extensions); packaged apps without one fall back to
//! their window's icon. Pixels come back premultiplied BGRA and the worker
//! pokes the window to repaint. A slow (network) path only stalls the
//! worker, never paint.
//!
//! Memory: 4 bytes per pixel per cached icon (1 KB at 100 % DPI, 4 KB at
//! 200 %), LRU-capped at `CAP` entries. D2D bitmaps live in `Gfx` and go
//! with the render target when the window hides.

use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_ICONS: u32 = WM_APP + 9;
const CAP: usize = 256;

/// Premultiplied BGRA, row-major, top row first.
pub struct Px {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u32>,
}

struct Job {
    key: Arc<str>,
    path: Arc<str>,
    pid: u32,
    size: u32,
}

#[derive(Default)]
struct Queue {
    jobs: Vec<Job>,
    done: Vec<(Arc<str>, Option<Arc<Px>>)>,
}

struct Worker {
    q: Mutex<Queue>,
    cv: Condvar,
}

static HWND_: AtomicIsize = AtomicIsize::new(0);
static WORKER: OnceLock<Arc<Worker>> = OnceLock::new();

/// Ask the window to repaint (from any thread): background results arrived.
pub fn poke() {
    let h = HWND_.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_ICONS, WPARAM(0), LPARAM(0));
        }
    }
}

/// The window the worker pokes (WM_APP_ICONS) when icons arrive.
pub fn set_hwnd(h: HWND) {
    HWND_.store(h.0 as isize, Ordering::Relaxed);
}

enum Slot {
    Pending,
    Missing,
    Ready(Arc<str>, Arc<Px>, u64),
}

#[derive(Default)]
pub struct Icons {
    map: HashMap<Arc<str>, Slot>,
    seq: u64,
}

impl Icons {
    /// Icon for `path` at `size` px, if extracted. Queues it on a miss.
    /// Returns the cache key (for the D2D bitmap cache) with the pixels.
    pub fn get(&mut self, path: &str, pid: u32, size: u32) -> Option<(Arc<str>, Arc<Px>)> {
        let path = source(path);
        self.seq += 1;
        let seq = self.seq;
        // keyed by size + lowercased path
        let mut k = String::with_capacity(path.len() + 6);
        k.push_str(&size.to_string());
        k.push('|');
        k.extend(path.chars().flat_map(|c| c.to_lowercase()));
        if let Some(slot) = self.map.get_mut(k.as_str()) {
            return match slot {
                Slot::Ready(key, px, used) => {
                    *used = seq;
                    Some((key.clone(), px.clone()))
                }
                _ => None,
            };
        }
        let key: Arc<str> = Arc::from(k);
        self.map.insert(key.clone(), Slot::Pending);
        let w = worker();
        w.q.lock().unwrap().jobs.push(Job { key, path: Arc::from(path.as_ref()), pid, size });
        w.cv.notify_one();
        None
    }

    /// Take finished extractions; call once per paint before `get`.
    pub fn pump(&mut self) {
        let Some(w) = WORKER.get() else { return };
        let done = std::mem::take(&mut w.q.lock().unwrap().done);
        if done.is_empty() {
            return;
        }
        for (key, px) in done {
            self.seq += 1;
            let slot = px.map(|p| Slot::Ready(key.clone(), p, self.seq)).unwrap_or(Slot::Missing);
            self.map.insert(key, slot);
        }
        // LRU: drop the least recently drawn icons over the cap
        let ready = self.map.values().filter(|s| matches!(s, Slot::Ready(..))).count();
        if ready > CAP {
            let mut used: Vec<u64> = self.map.values().filter_map(|s| if let Slot::Ready(_, _, u) = s { Some(*u) } else { None }).collect();
            used.sort_unstable();
            let cut = used[ready - CAP];
            self.map.retain(|_, s| !matches!(s, Slot::Ready(_, _, u) if *u < cut));
        }
    }

    /// Icons held (ready ones).
    pub fn len(&self) -> usize {
        self.map.values().filter(|s| matches!(s, Slot::Ready(..))).count()
    }

    /// Bytes of pixel data held (logged on hide).
    pub fn bytes(&self) -> usize {
        self.map.values().map(|s| if let Slot::Ready(_, p, _) = s { p.data.len() * 4 } else { 0 }).sum()
    }
}

/// Draws the icon of the image at `path` into the square `r` (DIPs, sized for
/// the window's DPI). False while it loads or without a path: the caller then
/// draws its own placeholder. Never blocks: a miss only queues the extraction.
pub fn draw(icons: &mut Icons, g: &mut crate::gfx::Gfx, path: &str, pid: u32, r: crate::gfx::Rect) -> bool {
    if path.is_empty() {
        return false;
    }
    let size = (r.w * g.dpi / 96.0).round().max(8.0) as u32;
    match icons.get(path, pid, size) {
        Some((key, px)) => {
            g.image(&key, px.w, px.h, &px.data, r);
            true
        }
        None => false,
    }
}

/// Where an image's icon comes from. svchost.exe has none (just the stock
/// application icon), so services it hosts share the Services console gears.
fn source(path: &str) -> std::borrow::Cow<'_, str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(r"\svchost.exe") || lower == "svchost.exe" {
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        return format!(r"{win}\System32\filemgmt.dll").into();
    }
    path.into()
}

fn worker() -> Arc<Worker> {
    WORKER
        .get_or_init(|| {
            let w = Arc::new(Worker { q: Mutex::new(Queue::default()), cv: Condvar::new() });
            let w2 = w.clone();
            let _ = std::thread::Builder::new().name("icons".into()).stack_size(256 * 1024).spawn(move || run(w2));
            w
        })
        .clone()
}

fn run(w: Arc<Worker>) {
    loop {
        let job = {
            let mut q = w.q.lock().unwrap();
            loop {
                // newest first: the rows on screen right now
                if let Some(j) = q.jobs.pop() {
                    break j;
                }
                q = w.cv.wait(q).unwrap();
            }
        };
        let px = extract(&job.path, job.pid, job.size).map(Arc::new);
        let idle = {
            let mut q = w.q.lock().unwrap();
            q.done.push((job.key, px));
            q.jobs.is_empty()
        };
        // one repaint per batch
        if idle {
            let h = HWND_.load(Ordering::Relaxed);
            if h != 0 {
                unsafe {
                    let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_ICONS, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
}

fn extract(path: &str, pid: u32, size: u32) -> Option<Px> {
    let wpath: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut icon = HICON::default();
        // MAKELONG(large size, small size): only the large one, at our size
        let hr = SHDefExtractIconW(PCWSTR(wpath.as_ptr()), 0, 0, Some(&mut icon), None, size & 0xffff);
        if hr == S_OK && !icon.is_invalid() {
            let px = icon_px(icon);
            let _ = DestroyIcon(icon);
            if px.is_some() {
                return px;
            }
        }
        // no icon resource (many packaged apps): the window's icon, not ours to destroy
        if let Some(px) = window_icon(pid, size).and_then(|h| icon_px(h)) {
            return Some(px);
        }
        // an exe without any icon (svchost, stubs): the stock application icon, like Task Manager
        let h = LoadImageW(None, IDI_APPLICATION, IMAGE_ICON, size as i32, size as i32, LR_SHARED).ok()?;
        icon_px(HICON(h.0))
    }
}

/// Icon of a visible top-level window of `pid`.
unsafe fn window_icon(pid: u32, size: u32) -> Option<HICON> {
    struct Find {
        pid: u32,
        hwnd: Option<HWND>,
    }
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        let f = &mut *(l.0 as *mut Find);
        let mut p = 0u32;
        GetWindowThreadProcessId(h, Some(&mut p));
        if p == f.pid && IsWindowVisible(h).as_bool() && GetWindow(h, GW_OWNER).map(|o| o.is_invalid()).unwrap_or(true) {
            f.hwnd = Some(h);
            return FALSE;
        }
        TRUE
    }
    let mut f = Find { pid, hwnd: None };
    let _ = EnumWindows(Some(cb), LPARAM(&mut f as *mut _ as isize));
    let h = f.hwnd?;
    let kinds: [usize; 3] = if size <= 24 { [ICON_SMALL2 as usize, ICON_SMALL as usize, ICON_BIG as usize] } else { [ICON_BIG as usize, ICON_SMALL2 as usize, ICON_SMALL as usize] };
    for k in kinds {
        let mut r = 0usize;
        // a hung window must not hang the worker for long
        let ok = SendMessageTimeoutW(h, WM_GETICON, WPARAM(k), LPARAM(96), SMTO_ABORTIFHUNG | SMTO_BLOCK, 200, Some(&mut r));
        if ok.0 != 0 && r != 0 {
            return Some(HICON(r as *mut _));
        }
    }
    for idx in if size <= 24 { [GCLP_HICONSM, GCLP_HICON] } else { [GCLP_HICON, GCLP_HICONSM] } {
        let r = GetClassLongPtrW(h, idx);
        if r != 0 {
            return Some(HICON(r as *mut _));
        }
    }
    None
}

/// HICON -> premultiplied BGRA. Icons without an alpha channel take it from the mask.
unsafe fn icon_px(icon: HICON) -> Option<Px> {
    let mut ii = ICONINFO::default();
    GetIconInfo(icon, &mut ii).ok()?;
    let out = (|| {
        if ii.hbmColor.is_invalid() {
            return None; // monochrome icon
        }
        let mut bm = BITMAP::default();
        if GetObjectW(HGDIOBJ(ii.hbmColor.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 || w > 256 || h > 256 {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let n = (w * h) as usize;
        let mut px = vec![0u32; n];
        let got = GetDIBits(dc, ii.hbmColor, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        if got == 0 {
            let _ = DeleteDC(dc);
            return None;
        }
        if !px.iter().any(|p| p >> 24 != 0) {
            // no alpha: opaque wherever the AND mask is 0
            let mut m = vec![0u32; n];
            let mut bi2 = bi;
            if GetDIBits(dc, ii.hbmMask, 0, h as u32, Some(m.as_mut_ptr() as *mut _), &mut bi2, DIB_RGB_COLORS) != 0 {
                for (p, m) in px.iter_mut().zip(&m) {
                    *p = (*p & 0xff_ffff) | if m & 0xff_ffff == 0 { 0xff00_0000 } else { 0 };
                }
            } else {
                for p in px.iter_mut() {
                    *p |= 0xff00_0000;
                }
            }
        }
        let _ = DeleteDC(dc);
        for p in px.iter_mut() {
            *p = premul(*p);
        }
        Some(Px { w: w as u32, h: h as u32, data: px })
    })();
    if !ii.hbmColor.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
    }
    if !ii.hbmMask.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
    }
    out
}

fn premul(p: u32) -> u32 {
    let a = p >> 24;
    let ch = |s: u32| (((p >> s) & 0xff) * a + 127) / 255;
    (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplies() {
        assert_eq!(premul(0xff10_2030), 0xff10_2030);
        assert_eq!(premul(0x00ff_ffff), 0);
        assert_eq!(premul(0x80ff_0000), 0x8080_0000);
    }

    #[test]
    fn extracts_explorer_icon_at_exact_size() {
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let px = extract(&format!("{win}\\explorer.exe"), 0, 32).expect("explorer has an icon");
        assert_eq!((px.w, px.h), (32, 32));
        assert!(px.data.iter().any(|p| p >> 24 == 0xff), "opaque pixels");
        assert!(px.data.iter().any(|p| p >> 24 == 0), "transparent corners");
    }

    #[test]
    fn svchost_services_share_the_services_icon() {
        assert!(source(r"C:\Windows\system32\svchost.exe").to_ascii_lowercase().ends_with(r"\system32\filemgmt.dll"));
        assert_eq!(source(r"C:\x\app.exe"), r"C:\x\app.exe");
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let px = extract(&source(&format!(r"{win}\System32\svchost.exe")), 0, 16).expect("gears");
        assert_eq!((px.w, px.h), (16, 16));
    }

    #[test]
    fn missing_file_gets_the_stock_icon() {
        let px = extract("C:\\definitely\\not\\here.exe", 0, 16).expect("stock icon");
        assert_eq!(px.data.len(), (px.w * px.h) as usize);
    }

    /// Cost per icon (`cargo test icon_bench -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn icon_bench() {
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let paths = ["explorer.exe", "notepad.exe", "System32\\svchost.exe", "System32\\taskmgr.exe", "System32\\mspaint.exe", "regedit.exe"];
        for p in paths {
            let t = std::time::Instant::now();
            let r = extract(&format!("{win}\\{p}"), 0, 32);
            println!("{p}: {:?} {:.2} ms", r.map(|p| (p.w, p.h)), t.elapsed().as_secs_f64() * 1e3);
        }
    }
}
