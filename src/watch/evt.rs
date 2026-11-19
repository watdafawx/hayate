//! System log subscription: Service Control Manager 7045 / 7036 / 7040 / 7034,
//! push via EvtSubscribe callback (0 CPU idle). A persisted bookmark catches
//! up on events from while Blink was not running.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::EventLog::*;

use crate::osutil::{self, wide};

const QUERY: &str = "<QueryList><Query Path=\"System\"><Select>*[System[Provider[@Name='Service Control Manager'] and (EventID=7045 or EventID=7036 or EventID=7040 or EventID=7034)]]</Select></Query></QueryList>";

/// One parsed SCM event.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ScmEvent {
    pub id: u32,
    pub record: u64,
    /// unix ms (TimeCreated)
    pub time: u64,
    pub user_sid: String,
    /// `<Data Name=..>` pairs in order
    pub data: Vec<(String, String)>,
    /// `<Binary>` hex
    pub binary: String,
}

impl ScmEvent {
    pub fn get(&self, name: &str) -> &str {
        self.data.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    }
    /// 7036 binary = UTF-16 "svcname/state" (4 running, 1 stopped).
    pub fn state_binary(&self) -> Option<(String, u32)> {
        let bytes: Vec<u8> = (0..self.binary.len() / 2).filter_map(|i| u8::from_str_radix(&self.binary[i * 2..i * 2 + 2], 16).ok()).collect();
        let w: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let s = osutil::from_wide(&w);
        let (name, st) = s.rsplit_once('/')?;
        Some((name.to_string(), st.trim().parse().ok()?))
    }
}

fn unxml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn tag_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}", tag);
    let i = xml.find(&open)?;
    let rest = &xml[i + open.len()..];
    // must be the tag itself, not a prefix (EventID vs EventIDx)
    if !rest.starts_with(['>', ' ', '/']) {
        return None;
    }
    let gt = rest.find('>')?;
    if rest[..gt].ends_with('/') {
        return Some("");
    }
    let body = &rest[gt + 1..];
    let end = body.find(&format!("</{}>", tag))?;
    Some(&body[..end])
}

fn attr<'a>(xml: &'a str, tag: &str, name: &str) -> Option<&'a str> {
    let open = format!("<{} ", tag);
    let i = xml.find(&open)?;
    let rest = &xml[i + open.len()..];
    let gt = rest.find('>')?;
    let a = &rest[..gt];
    for q in ['\'', '"'] {
        let key = format!("{}={}", name, q);
        if let Some(j) = a.find(&key) {
            let v = &a[j + key.len()..];
            return v.find(q).map(|e| &v[..e]);
        }
    }
    None
}

/// ISO-8601 `2026-11-15T10:20:30.1234567Z` → unix ms.
pub fn parse_systemtime(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |a: usize, l: usize| s.get(a..a + l)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?, n(17, 2)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut ms = 0i64;
    if s.as_bytes().get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
        if !frac.is_empty() {
            ms = frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
        }
    }
    let t = ((days * 86400 + h * 3600 + mi * 60 + se) * 1000) + ms;
    (t >= 0).then_some(t as u64)
}

pub fn parse_event_xml(xml: &str) -> Option<ScmEvent> {
    let id: u32 = tag_text(xml, "EventID")?.trim().parse().ok()?;
    let record = tag_text(xml, "EventRecordID").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let time = attr(xml, "TimeCreated", "SystemTime").and_then(parse_systemtime).unwrap_or(0);
    let user_sid = attr(xml, "Security", "UserID").unwrap_or("").to_string();
    let mut data = Vec::new();
    if let Some(ed) = tag_text(xml, "EventData") {
        let mut rest = ed;
        while let Some(i) = rest.find("<Data") {
            rest = &rest[i + 5..];
            let gt = match rest.find('>') {
                Some(g) => g,
                None => break,
            };
            let head = &rest[..gt];
            let name = ["Name='", "Name=\""].iter().find_map(|k| head.find(k).map(|j| (j + k.len(), k.chars().last().unwrap()))).and_then(|(j, q)| head[j..].find(q).map(|e| head[j..j + e].to_string())).unwrap_or_default();
            if head.ends_with('/') {
                data.push((name, String::new()));
                rest = &rest[gt + 1..];
                continue;
            }
            let body = &rest[gt + 1..];
            let end = body.find("</Data>").unwrap_or(body.len());
            data.push((name, unxml(&body[..end])));
            rest = &body[end.min(body.len())..];
        }
    }
    let binary = tag_text(xml, "Binary").unwrap_or("").trim().to_string();
    Some(ScmEvent { id, record, time, user_sid, data, binary })
}

// ---- subscription ----------------------------------------------------------------

static SINK: OnceLock<Mutex<Sender<super::Msg>>> = OnceLock::new();
static BOOKMARK: AtomicIsize = AtomicIsize::new(0);
static LAST_SAVE: AtomicU64 = AtomicU64::new(0);
static SUB: AtomicIsize = AtomicIsize::new(0);

fn bookmark_file() -> std::path::PathBuf {
    osutil::data_dir().join("watch-bookmark.xml")
}

/// RecordId of a System-channel bookmark; None for anything else (empty
/// `<BookmarkList>`, other channel, garbage, truncated file).
pub fn bookmark_record(xml: &str) -> Option<u64> {
    let ch = attr(xml, "Bookmark", "Channel")?;
    if !ch.eq_ignore_ascii_case("System") {
        return None;
    }
    attr(xml, "Bookmark", "RecordId")?.trim().parse().ok()
}

/// How the subscription started.
#[derive(Clone, Debug, PartialEq)]
pub enum Resume {
    /// resumed after a valid bookmark last saved at `saved_ms` (unix ms)
    Bookmark { record: u64, saved_ms: u64 },
    /// no usable bookmark: future events only (reason for the log)
    Fresh(String),
}

impl Resume {
    pub fn saved_ms(&self) -> Option<u64> {
        match self {
            Resume::Bookmark { saved_ms, .. } => Some(*saved_ms),
            Resume::Fresh(_) => None,
        }
    }
}

fn render(h: EVT_HANDLE, flags: u32) -> Option<String> {
    unsafe {
        let mut used = 0u32;
        let mut props = 0u32;
        let _ = EvtRender(None, h, flags, 0, None, &mut used, &mut props);
        if used == 0 {
            return None;
        }
        let mut buf = vec![0u16; used as usize / 2 + 1];
        EvtRender(None, h, flags, (buf.len() * 2) as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut used, &mut props).ok()?;
        Some(osutil::from_wide(&buf))
    }
}

/// Atomic write with a per-process temp name, so two Blink instances
/// (user + admin) saving at once never interleave into one temp file.
fn write_bookmark(x: &str) {
    let f = bookmark_file();
    let tmp = f.with_extension(format!("xml.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, x.as_bytes()).is_ok() && std::fs::rename(&tmp, &f).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Save decision (pure). Never write an empty/invalid bookmark; never move
/// the shared file backwards (another instance is further ahead) unless
/// `overwrite` (fresh start: the old one was unusable).
pub fn should_save(ours: Option<u64>, on_disk: Option<u64>, overwrite: bool) -> bool {
    match ours {
        None => false,
        Some(r) => overwrite || on_disk.is_none_or(|d| r >= d),
    }
}

fn save(force: bool, overwrite: bool) {
    let b = BOOKMARK.load(Ordering::SeqCst);
    if b == 0 {
        return;
    }
    let now = osutil::now_unix();
    if !force && now.saturating_sub(LAST_SAVE.load(Ordering::Relaxed)) < 10 {
        return;
    }
    LAST_SAVE.store(now, Ordering::Relaxed);
    let Some(x) = render(EVT_HANDLE(b), EvtRenderBookmark.0) else { return };
    let ours = bookmark_record(&x);
    let disk = std::fs::read_to_string(bookmark_file()).ok().and_then(|s| bookmark_record(&s));
    if should_save(ours, disk, overwrite) {
        write_bookmark(&x);
    } else if force && ours.is_some() && disk.is_some() {
        // another instance is ahead: keep its file, but stamp this session's
        // end (mtime = "previous session ended" for the catch-up gate)
        let _ = std::fs::File::options().append(true).open(bookmark_file()).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    }
}

/// Persist the bookmark (throttled to once per 10 s unless `force`).
pub fn save_bookmark(force: bool) {
    save(force, false);
}

unsafe extern "system" fn on_evt(action: EVT_SUBSCRIBE_NOTIFY_ACTION, _ctx: *const c_void, ev: EVT_HANDLE) -> u32 {
    if action != EvtSubscribeActionDeliver {
        return 0;
    }
    if let Some(xml) = render(ev, EvtRenderEventXml.0) {
        let b = BOOKMARK.load(Ordering::SeqCst);
        if b != 0 {
            let _ = EvtUpdateBookmark(EVT_HANDLE(b), ev);
        }
        if let Some(e) = parse_event_xml(&xml) {
            if let Some(s) = SINK.get() {
                let _ = s.lock().unwrap().send(super::Msg::Scm(e));
            }
        }
        save_bookmark(false);
    }
    0
}

pub fn bookmark_xml(record: u64) -> String {
    format!("<BookmarkList>\r\n  <Bookmark Channel='System' RecordId='{}' IsCurrent='true'/>\r\n</BookmarkList>", record)
}

/// Record id of the `skip`-th newest matching event (0 = newest).
unsafe fn newest_record(skip: u32) -> Option<u64> {
    let ch = wide("System");
    let q = wide(QUERY);
    let rs = EvtQuery(None, osutil::pcw(&ch), osutil::pcw(&q), EvtQueryChannelPath.0 | EvtQueryReverseDirection.0).ok()?;
    let mut out = None;
    if skip == 0 || EvtSeek(rs, skip as i64, None, None, EvtSeekRelativeToFirst.0).is_ok() {
        let mut evs = [0isize; 1];
        let mut n = 0u32;
        if EvtNext(rs, &mut evs, 0, 0, &mut n).is_ok() && n == 1 {
            out = render(EVT_HANDLE(evs[0]), EvtRenderEventXml.0).and_then(|x| parse_event_xml(&x)).map(|e| e.record).filter(|&r| r != 0);
            let _ = EvtClose(EVT_HANDLE(evs[0]));
        }
    }
    let _ = EvtClose(rs);
    out
}

/// Fresh-start bookmark at the newest matching record, so a valid bookmark
/// is on disk right away (an exit before the first event no longer leaves an
/// empty `<BookmarkList>` that would replay the whole log next time).
unsafe fn fresh_bookmark() -> Result<(EVT_HANDLE, bool), String> {
    if let Some(r) = newest_record(0) {
        let w = wide(&bookmark_xml(r));
        if let Ok(bm) = EvtCreateBookmark(osutil::pcw(&w)) {
            return Ok((bm, true));
        }
    }
    EvtCreateBookmark(windows::core::PCWSTR::null()).map(|b| (b, false)).map_err(|e| format!("EvtCreateBookmark: {}", e.message()))
}

/// Subscribe. Resumes strictly after the saved bookmark; if that record is
/// gone (log wrapped / cleared), the file is not a System bookmark, or
/// anything else fails, starts with future events only and saves a fresh
/// bookmark. Without `EvtSubscribeStrict` Windows silently starts at the
/// OLDEST record instead, replaying the whole System log as new.
pub fn start(tx: Sender<super::Msg>) -> Result<Resume, String> {
    let _ = SINK.set(Mutex::new(tx));
    let file = bookmark_file();
    let saved = std::fs::read_to_string(&file).ok();
    let saved_ms = std::fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
    let ch = wide("System");
    let q = wide(QUERY);
    unsafe {
        let reason = match saved.as_deref().map(|x| (x, bookmark_record(x))) {
            None => "no bookmark".to_string(),
            Some((x, None)) => format!("invalid bookmark ({} bytes)", x.len()),
            Some((x, Some(record))) => {
                let w = wide(x);
                match EvtCreateBookmark(osutil::pcw(&w)) {
                    Err(e) => format!("EvtCreateBookmark: {}", e.message()),
                    Ok(bm) => match EvtSubscribe(None, None, osutil::pcw(&ch), osutil::pcw(&q), Some(bm), None, Some(on_evt), EvtSubscribeStartAfterBookmark.0 | EvtSubscribeStrict.0) {
                        Ok(sub) => {
                            BOOKMARK.store(bm.0, Ordering::SeqCst);
                            SUB.store(sub.0, Ordering::SeqCst);
                            return Ok(Resume::Bookmark { record, saved_ms: saved_ms.unwrap_or(0) });
                        }
                        Err(e) => {
                            let _ = EvtClose(bm);
                            format!("bookmark record {} unusable: {} (0x{:08x})", record, e.message(), e.code().0 as u32)
                        }
                    },
                }
            }
        };
        let (bm, seeded) = fresh_bookmark()?;
        BOOKMARK.store(bm.0, Ordering::SeqCst);
        let sub = EvtSubscribe(None, None, osutil::pcw(&ch), osutil::pcw(&q), None, None, Some(on_evt), EvtSubscribeToFutureEvents.0).map_err(|e| format!("EvtSubscribe: {}", e.message()))?;
        SUB.store(sub.0, Ordering::SeqCst);
        if seeded {
            save(true, true);
        }
        Ok(Resume::Fresh(reason))
    }
}

pub fn stop() {
    save_bookmark(true);
    let s = SUB.swap(0, Ordering::SeqCst);
    if s != 0 {
        unsafe {
            let _ = EvtClose(EVT_HANDLE(s));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X7045: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='Service Control Manager' Guid='{555908d1-a6d7-4695-8e1e-26931d2012f4}' EventSourceName='Service Control Manager'/><EventID Qualifiers='16384'>7045</EventID><Version>0</Version><Level>4</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8080000000000000</Keywords><TimeCreated SystemTime='2026-11-15T10:20:30.1234567Z'/><EventRecordID>123456</EventRecordID><Correlation/><Execution ProcessID='1024' ThreadID='2048'/><Channel>System</Channel><Computer>dev PC</Computer><Security UserID='S-1-5-18'/></System><EventData><Data Name='ServiceName'>NEPb67ocmj</Data><Data Name='ImagePath'>\\SystemRoot\\System32\\drivers\\NEPb67ocmj.sys</Data><Data Name='ServiceType'>kernel mode driver</Data><Data Name='StartType'>demand start</Data><Data Name='AccountName'></Data></EventData></Event>";

    const X7036: &str = "<Event><System><EventID Qualifiers='16384'>7036</EventID><TimeCreated SystemTime='2026-11-15T10:20:30Z'/><EventRecordID>7</EventRecordID></System><EventData><Data Name='param1'>Windows Update</Data><Data Name='param2'>running</Data></EventData><Binary>770075006100750073006500720076002F0034000000</Binary></Event>";

    #[test]
    fn parse_7045() {
        let e = parse_event_xml(X7045).unwrap();
        assert_eq!(e.id, 7045);
        assert_eq!(e.record, 123456);
        assert_eq!(e.user_sid, "S-1-5-18");
        assert_eq!(e.get("ServiceName"), "NEPb67ocmj");
        assert_eq!(e.get("ImagePath"), "\\SystemRoot\\System32\\drivers\\NEPb67ocmj.sys");
        assert_eq!(e.get("AccountName"), "");
        assert_eq!(e.time, parse_systemtime("2026-11-15T10:20:30.123Z").unwrap());
    }

    #[test]
    fn parse_7036_binary() {
        let e = parse_event_xml(X7036).unwrap();
        assert_eq!(e.state_binary(), Some(("wuauserv".to_string(), 4)));
        assert_eq!(e.get("param1"), "Windows Update");
    }

    #[test]
    fn systemtime() {
        assert_eq!(parse_systemtime("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_systemtime("2000-03-01T00:00:00.5Z"), Some(951_868_800_500));
        assert_eq!(parse_systemtime("junk"), None);
    }

    #[test]
    fn entities() {
        let e = parse_event_xml("<Event><EventID>7040</EventID><EventData><Data Name='param1'>A &amp; B &lt;x&gt;</Data><Data Name='param4'>ab</Data></EventData></Event>").unwrap();
        assert_eq!(e.get("param1"), "A & B <x>");
        assert_eq!(e.get("param4"), "ab");
    }

    #[test]
    fn bookmark_validation() {
        let ok = "<BookmarkList>\r\n  <Bookmark Channel='System' RecordId='2142712' IsCurrent='true'/>\r\n</BookmarkList>";
        assert_eq!(bookmark_record(ok), Some(2142712));
        // what an untouched EvtCreateBookmark(NULL) renders to: used to pass
        // the old `contains("<Bookmark")` check and replay the whole log
        assert_eq!(bookmark_record("<BookmarkList>\r\n</BookmarkList>"), None);
        assert_eq!(bookmark_record("<BookmarkList><Bookmark Channel='Application' RecordId='5'/></BookmarkList>"), None);
        assert_eq!(bookmark_record("<BookmarkList><Bookmark Channel='System' RecordId='"), None);
        assert_eq!(bookmark_record(""), None);
    }

    #[test]
    fn save_rules() {
        assert!(!should_save(None, None, false)); // empty bookmark: never write
        assert!(!should_save(None, Some(5), true));
        assert!(should_save(Some(5), None, false));
        assert!(should_save(Some(9), Some(5), false));
        assert!(!should_save(Some(5), Some(9), false)); // other instance ahead
        assert!(should_save(Some(5), Some(9), true)); // fresh start replaces stale file
    }

    static PROBE_N: AtomicU64 = AtomicU64::new(0);
    unsafe extern "system" fn probe_cb(action: EVT_SUBSCRIBE_NOTIFY_ACTION, _c: *const c_void, ev: EVT_HANDLE) -> u32 {
        if action == EvtSubscribeActionDeliver {
            PROBE_N.fetch_add(1, Ordering::SeqCst);
        } else {
            eprintln!("  callback action={} code={}", action.0, ev.0);
        }
        0
    }
    fn probe(label: &str, flags: u32, bm: Option<&str>) {
        PROBE_N.store(0, Ordering::SeqCst);
        unsafe {
            let h = match bm {
                Some(x) => {
                    let w = wide(x);
                    EvtCreateBookmark(osutil::pcw(&w))
                }
                None => EvtCreateBookmark(windows::core::PCWSTR::null()),
            };
            let h = match h {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("{label}: EvtCreateBookmark err {:?}", e);
                    return;
                }
            };
            let ch = wide("System");
            let q = wide(QUERY);
            let r = EvtSubscribe(None, None, osutil::pcw(&ch), osutil::pcw(&q), if bm.is_some() || flags & 3 == 3 { Some(h) } else { None }, None, Some(probe_cb), flags);
            match r {
                Ok(s) => {
                    std::thread::sleep(std::time::Duration::from_secs(4));
                    let _ = EvtClose(s);
                    eprintln!("{label}: delivered {}", PROBE_N.load(Ordering::SeqCst));
                }
                Err(e) => eprintln!("{label}: EvtSubscribe err {:?} (0x{:08x})", e.message(), e.code().0),
            }
            let _ = EvtClose(h);
        }
    }

    /// Headless probe of EvtSubscribe start modes against the real System log.
    #[test]
    #[ignore]
    fn evt_probe() {
        let fut = EvtSubscribeToFutureEvents.0;
        let after = EvtSubscribeStartAfterBookmark.0;
        let strict = EvtSubscribeStrict.0;
        probe("future, no bm", fut, None);
        probe("after, empty bm", after, None);
        probe("after, empty bm, strict", after | strict, None);
        let gone = "<BookmarkList><Bookmark Channel='System' RecordId='5' IsCurrent='true'/></BookmarkList>";
        probe("after, gone rec", after, Some(gone));
        probe("after, gone rec, strict", after | strict, Some(gone));
        let other = "<BookmarkList><Bookmark Channel='Application' RecordId='5' IsCurrent='true'/></BookmarkList>";
        probe("after, other channel", after, Some(other));
        probe("after, other channel, strict", after | strict, Some(other));
        let far = "<BookmarkList><Bookmark Channel='System' RecordId='999999999' IsCurrent='true'/></BookmarkList>";
        probe("after, future rec", after, Some(far));
        probe("after, future rec, strict", after | strict, Some(far));
        probe("after, garbage", after, Some("<Bookmark junk"));
        unsafe {
            let e = EvtCreateBookmark(windows::core::PCWSTR::null()).unwrap();
            eprintln!("empty bookmark renders: {:?}", render(e, EvtRenderBookmark.0));
            let _ = EvtClose(e);
            let (f, seeded) = fresh_bookmark().unwrap();
            let x = render(f, EvtRenderBookmark.0).unwrap();
            eprintln!("fresh_bookmark seeded={seeded}: {:?} -> {:?}", x, bookmark_record(&x));
            let _ = EvtClose(f);
            probe("after, fresh, strict", after | strict, Some(&x));
            let r = newest_record(10).unwrap();
            eprintln!("11th newest record {r}");
            probe("after, 11th newest, strict (expect 10)", after | strict, Some(&bookmark_xml(r)));
        }
    }

    /// Elevated or not (System log is readable by users): subscribe, then
    /// `sc config <svc> start= demand` style changes show up. Manual check.
    #[test]
    #[ignore]
    fn evt_live() {
        let (tx, rx) = std::sync::mpsc::channel();
        start(tx).unwrap();
        eprintln!("subscribed; waiting 20 s for SCM events");
        if let Ok(m) = rx.recv_timeout(std::time::Duration::from_secs(20)) {
            if let super::super::Msg::Scm(e) = m {
                eprintln!("{:?}", e);
            }
        }
        stop();
    }
}
