//! Blink's identity for Windows notifications, how Watch alerts reach the
//! user, and whether Windows will show them right now.
//!
//! Blink sets an explicit AppUserModelID; the installer (Settings > Install, or
//! install.ps1) stamps the same ID on its Start-menu shortcut, so toasts, the
//! Settings > Notifications entry and the taskbar button all resolve to
//! "Blink" with Blink's icon.
//!
//! Alerts are real Windows toasts (WinRT ToastNotificationManager) when that
//! shortcut exists: they stay in the notification centre until dismissed, even
//! under Do not disturb. Without it (dev runs, test copies) Windows drops
//! toasts from an unregistered ID, so `toast` returns false and the caller
//! shows a tray balloon (Shell_NotifyIcon NIF_INFO) instead.
//!
//! Clicks on the pop-up arrive in-process (the toast's Activated event) and
//! open Watch on the event. Clicks on a toast already in the notification
//! centre go to the COM activator the installer registers (one CLSID under
//! HKCU\Software\Classes, named by ToastActivatorCLSID on Blink.lnk): Windows
//! starts `blink.exe -ToastActivated`, which hands the event to the running
//! Blink or starts Blink on Watch (see `crate::setup`). A portable Blink has
//! no activator, so there such clicks only dismiss.

use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::System::Registry::*;

/// Keep in sync with install.ps1 (`$appId`).
pub const AUMID: &str = "Blink.TaskManager";

/// Debug builds only: `BLINK_TEST_AUMID=Blink.TaskManager.Test<something>` gives
/// this process a disposable app identity, so a test of the taskbar Jump
/// List never touches the real "Blink.TaskManager" list or the user's pins.
/// Release builds ignore it.
pub fn test_aumid() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        let v = std::env::var("BLINK_TEST_AUMID").ok()?;
        let ok = v.len() > "Blink.TaskManager.Test".len() && v.len() <= 100 && v.starts_with("Blink.TaskManager.Test") && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
        return ok.then_some(v);
    }
    #[cfg(not(debug_assertions))]
    None
}

/// The app ID this process runs under: [`AUMID`], or the debug test identity.
pub fn aumid() -> String {
    test_aumid().unwrap_or_else(|| AUMID.to_string())
}

/// Call once at start, before any window exists.
pub fn set_app_id() {
    let w: Vec<u16> = aumid().encode_utf16().chain(Some(0)).collect();
    let r = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(PCWSTR(w.as_ptr())) };
    if let Err(e) = r {
        crate::osutil::log(&format!("app id: {e}"));
    }
}

/// One alert for the notification centre.
pub struct Note {
    pub title: String,
    pub text: String,
    /// event kind (Watch alert key): one toast per kind stays in the
    /// notification centre and a newer one replaces it, so floods collapse
    pub kind: &'static str,
    /// Watch event to open on click
    pub event_id: Option<u64>,
}

type Click = Box<dyn Fn(u64) + Send + Sync>;
static CLICK: OnceLock<Click> = OnceLock::new();
/// The toast thread once toasts work; while they do not, when we last asked.
static TOASTER: std::sync::Mutex<(Option<Sender<Note>>, Option<std::time::Instant>)> = std::sync::Mutex::new((None, None));

/// A shortcut written moments ago (self-install, then start the installed
/// copy) reaches shell:AppsFolder a few seconds later: ask again after this.
const RECHECK: std::time::Duration = std::time::Duration::from_secs(15);

/// What a toast click does: called on a WinRT worker thread with the Watch
/// event id (0 for none). Set once, before the first toast.
pub fn on_click(f: impl Fn(u64) + Send + Sync + 'static) {
    let _ = CLICK.set(Box::new(f));
}

/// Real toasts work here: Blink's AUMID belongs to a Start-menu shortcut.
/// The first call asks the shell (tens of ms): never on the UI thread.
pub fn toasts_available() -> bool {
    toaster().is_some()
}

/// The toast thread's queue. A "no" is not final: it is asked again after
/// RECHECK, so a copy started right after its own install gets toasts.
fn toaster() -> Option<Sender<Note>> {
    let mut g = TOASTER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = &g.0 {
        return Some(tx.clone());
    }
    if g.1.is_some_and(|t| t.elapsed() < RECHECK) {
        return None;
    }
    let first = g.1.is_none();
    g.1 = Some(std::time::Instant::now());
    g.0 = start(first);
    g.0.clone()
}

/// Queue a Windows toast. False when toasts are not available (no
/// shortcut): the caller shows a balloon instead. Shown on the "toast" thread,
/// so the caller never waits on WinRT; the first call does wait for the
/// availability check: never call it on the UI thread.
pub fn toast(n: Note) -> bool {
    match toaster() {
        Some(tx) => tx.send(n).is_ok(),
        None => false,
    }
}

fn start(first: bool) -> Option<Sender<Note>> {
    let (tx, rx) = std::sync::mpsc::channel::<Note>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<bool>(1);
    let spawned = std::thread::Builder::new().name("toast".into()).stack_size(256 * 1024).spawn(move || {
        use windows::UI::Notifications::ToastNotificationManager;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        // WinRT needs an apartment; MTA: the Activated callbacks need no message pump
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let notifier = if registered() { ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)).ok() } else { None };
        if notifier.is_some() {
            crate::osutil::log("toasts: Windows toasts (Start-menu shortcut found)");
        } else if first {
            crate::osutil::log("toasts: tray balloons (no Start-menu shortcut with Blink's app ID; asked again every 15 s)");
        }
        let _ = ready_tx.send(notifier.is_some());
        let Some(notifier) = notifier else { return };
        let logo = logo_uri();
        // a toast's Activated handler lives with the toast object: keep the recent ones
        let mut keep = std::collections::VecDeque::new();
        for n in rx {
            match show(&notifier, &n, logo.as_deref()) {
                Ok(t) => {
                    keep.push_back(t);
                    if keep.len() > 64 {
                        keep.pop_front();
                    }
                }
                Err(e) => crate::osutil::log(&format!("toast failed: {e} title={:?}", n.title)),
            }
        }
    });
    if spawned.is_err() {
        return None;
    }
    ready_rx.recv().ok()?.then_some(tx)
}

/// Windows lists an AUMID in its app list (shell:AppsFolder) only when a
/// Start-menu shortcut carries it, the condition for desktop-app toasts.
fn registered() -> bool {
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName};
    let path = HSTRING::from(format!(r"shell:AppsFolder\{AUMID}"));
    unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(&path, None) }.is_ok()
}

fn show(notifier: &windows::UI::Notifications::ToastNotifier, n: &Note, logo: Option<&str>) -> windows::core::Result<windows::UI::Notifications::ToastNotification> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::*;
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(toast_xml(n, logo)))?;
    let t = ToastNotification::CreateToastNotification(&doc)?;
    t.SetTag(&HSTRING::from(n.kind))?;
    t.SetGroup(&HSTRING::from("watch"))?;
    t.Activated(&TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(|_, args| {
        let arg = args.as_ref().and_then(|a| a.cast::<ToastActivatedEventArgs>().ok()).and_then(|a| a.Arguments().ok()).map(|s| s.to_string()).unwrap_or_default();
        crate::osutil::log(&format!("toast clicked {arg:?}"));
        if let Some(f) = CLICK.get() {
            f(parse_launch(&arg).unwrap_or(0));
        }
        Ok(())
    }))?;
    t.Failed(&TypedEventHandler::<ToastNotification, ToastFailedEventArgs>::new(|_, args| {
        if let Some(a) = args.as_ref() {
            crate::osutil::log(&format!("toast failed: {:?}", a.ErrorCode()));
        }
        Ok(())
    }))?;
    notifier.Show(&t)?;
    crate::osutil::log(&format!("toast shown kind={} title={:?} text={:?}", n.kind, n.title, n.text));
    Ok(t)
}

/// ToastGeneric: title, text, Blink's logo; `launch` names the event.
fn toast_xml(n: &Note, logo: Option<&str>) -> String {
    let launch = match n.event_id {
        Some(id) => format!("watch:{id}"),
        None => "watch".into(),
    };
    let img = logo.map(|u| format!(r#"<image placement="appLogoOverride" src="{}"/>"#, esc(u))).unwrap_or_default();
    format!(r#"<toast launch="{}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text>{img}</binding></visual></toast>"#, esc(&launch), esc(&n.title), esc(&n.text))
}

pub fn parse_launch(s: &str) -> Option<u64> {
    s.strip_prefix("watch:")?.parse().ok()
}

/// XML text/attribute escape; drops control characters XML 1.0 forbids.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            '\n' | '\t' => o.push(c),
            c if (c as u32) < 0x20 => {}
            c => o.push(c),
        }
    }
    o
}

/// Toast images must be files: writes the 256 px PNG of Blink's own icon
/// (the exe's icon resource, no second copy in the binary) once to
/// %LOCALAPPDATA%\Blink\toast-logo.png and returns its file URI.
fn logo_uri() -> Option<String> {
    let png = icon_png()?;
    let p = crate::osutil::data_dir().join("toast-logo.png");
    if std::fs::metadata(&p).map(|m| m.len() as usize != png.len()).unwrap_or(true) {
        let _ = std::fs::create_dir_all(p.parent()?);
        std::fs::write(&p, png).ok()?;
    }
    let s = p.to_string_lossy().replace('\\', "/").replace(' ', "%20");
    Some(format!("file:///{s}"))
}

/// The PNG-compressed 256 px image of the exe's icon group 1.
fn icon_png() -> Option<&'static [u8]> {
    use windows::Win32::System::LibraryLoader::*;
    unsafe {
        let m = GetModuleHandleW(None).ok()?;
        let res = |id: u16, ty: u16| -> Option<&'static [u8]> {
            let r = FindResourceW(Some(m), PCWSTR(id as usize as *const u16), PCWSTR(ty as usize as *const u16));
            if r.is_invalid() {
                return None;
            }
            let n = SizeofResource(Some(m), r) as usize;
            let p = LockResource(LoadResource(Some(m), r).ok()?) as *const u8;
            (!p.is_null() && n > 0).then(|| std::slice::from_raw_parts(p, n))
        };
        // RT_GROUP_ICON (14): 6-byte header, then 14-byte entries (width 0 = 256 px)
        let g = res(1, 14)?;
        let count = u16::from_le_bytes([*g.get(4)?, *g.get(5)?]) as usize;
        (0..count).find_map(|i| {
            let e = g.get(6 + i * 14..6 + i * 14 + 14)?;
            if e[0] != 0 {
                return None;
            }
            // RT_ICON (3) holds the image as stored in the .ico
            let img = res(u16::from_le_bytes([e[12], e[13]]), 3)?;
            img.starts_with(b"\x89PNG").then_some(img)
        })
    }
}

/// Why Windows would not pop a toast up right now (two short lines for the
/// Watch panel), or None when it shows them. Reads two HKCU values, the Do not
/// disturb state and the shell's notification state; the last can wait on
/// Explorer, so call it off the UI thread.
pub fn blocked_reason() -> Option<String> {
    let base = r"Software\Microsoft\Windows\CurrentVersion";
    if reg_dword(&format!(r"{base}\PushNotifications"), "ToastEnabled") == Some(0) {
        return Some("Windows notifications are off\nSettings > System > Notifications".into());
    }
    if reg_dword(&format!(r"{base}\Notifications\Settings\{AUMID}"), "Enabled") == Some(0) {
        return Some("Notifications are off for Hayate\nSettings > System > Notifications".into());
    }
    if do_not_disturb() {
        return Some(if toasts_available() { "Do not disturb is on\nalerts wait in the notification centre" } else { "Do not disturb is on\nalerts do not pop up (nor are kept)" }.into());
    }
    use windows::Win32::UI::Shell::*;
    // QUNS_BUSY is left out: a topmost full-screen window reports it, yet toasts still show
    match unsafe { SHQueryUserNotificationState() }.ok()? {
        QUNS_RUNNING_D3D_FULL_SCREEN => Some("A full-screen game is running\nalerts may not pop up".into()),
        QUNS_PRESENTATION_MODE => Some("Presentation mode is on\nalerts do not pop up".into()),
        QUNS_QUIET_TIME => Some("Windows quiet time (new account)\nalerts are held for an hour".into()),
        _ => None,
    }
}

/// Do not disturb (Windows 11; Focus assist "priority/alarms only" on 10),
/// including its automatic rules (gaming, full screen, duplicating a display).
/// No documented API exists: this reads the shell's well-known WNF state
/// WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED (0 off, 1 priority only,
/// 2 alarms only), read-only, through ntdll resolved at run time.
fn do_not_disturb() -> bool {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type Query = unsafe extern "system" fn(*const u64, *const core::ffi::c_void, *const core::ffi::c_void, *mut u32, *mut core::ffi::c_void, *mut u32) -> i32;
    const NAME: u64 = 0x0D83_063E_A3BF_1C75;
    unsafe {
        let Ok(ntdll) = GetModuleHandleW(w!("ntdll.dll")) else { return false };
        let Some(p) = GetProcAddress(ntdll, s!("NtQueryWnfStateData")) else { return false };
        let query: Query = std::mem::transmute::<unsafe extern "system" fn() -> isize, Query>(p);
        let (mut stamp, mut val, mut size) = (0u32, 0u32, 4u32);
        let st = query(&NAME, std::ptr::null(), std::ptr::null(), &mut stamp, &mut val as *mut u32 as *mut _, &mut size);
        st >= 0 && size >= 4 && val != 0
    }
}

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    let k: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let v: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut d = 0u32;
    let mut n = 4u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_DWORD, None, Some(&mut d as *mut _ as *mut _), Some(&mut n)) };
    r.is_ok().then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_values_read_as_none() {
        assert_eq!(reg_dword(r"Software\Blink\definitely-not-here", "x"), None);
    }

    #[test]
    fn toast_xml_escapes_and_names_the_event() {
        let n = Note { title: "a<b & \"c\"".into(), text: "x\u{1}y".into(), kind: "new_app", event_id: Some(42) };
        let x = toast_xml(&n, Some("file:///C:/a%20b/l.png"));
        assert!(x.contains(r#"launch="watch:42""#));
        assert!(x.contains("<text>a&lt;b &amp; &quot;c&quot;</text><text>xy</text>"));
        assert!(x.contains(r#"src="file:///C:/a%20b/l.png""#));
        assert_eq!(parse_launch("watch:42"), Some(42));
        assert_eq!(parse_launch("watch"), None);
    }

    #[test]
    fn state_query_does_not_panic() {
        let _ = blocked_reason();
    }
}
