//! The account a process runs as, asked for lazily.
//!
//! Not part of the per-process details every row gets: resolving an account
//! name can reach a domain controller, so it is looked up only for the one
//! process a hover card is about, on the background I/O worker, and kept per
//! process identity (pid + create time, so a reused pid never inherits an
//! answer). The UI never waits: it shows "looking up" until the answer comes.
//!
//! Uses only the access Blink already has: a limited-information process
//! handle and a query-only token. No privilege is enabled, no elevation is
//! requested, nothing is injected. A process whose token cannot be opened
//! (another user's, protected) answers `Unavailable`, which is the honest
//! result, not an error to work around.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::Security::{GetTokenInformation, LookupAccountSidW, TokenElevation, TokenUser, SID_NAME_USE, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};

#[derive(Clone, Debug, PartialEq)]
pub enum User {
    /// asked for, no answer yet
    Pending,
    /// `DOMAIN\name`; `elevated` is the token's own flag (None when not readable)
    Known { name: Arc<str>, elevated: Option<bool> },
    /// the reason, in words for the card
    Unavailable(&'static str),
}

/// Answers kept (oldest dropped first); asked-and-waiting lookups at once.
const KEEP: usize = 128;
const MAX_PENDING: usize = 8;

struct Entry {
    user: User,
    seq: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<(u32, i64), Entry>,
    seq: u64,
}

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// The cached answer for this exact process, queueing the lookup the first
/// time. Never blocks.
pub fn lookup(pid: u32, create: i64) -> User {
    {
        let mut c = cache();
        if let Some(e) = c.map.get(&(pid, create)) {
            return e.user.clone();
        }
        if c.map.values().filter(|e| e.user == User::Pending).count() >= MAX_PENDING {
            return User::Pending; // asked again at the next paint
        }
        c.seq += 1;
        let seq = c.seq;
        if c.map.len() >= KEEP {
            if let Some(old) = c.map.iter().filter(|(_, e)| e.user != User::Pending).min_by_key(|(_, e)| e.seq).map(|(k, _)| *k) {
                c.map.remove(&old);
            }
        }
        c.map.insert((pid, create), Entry { user: User::Pending, seq });
    }
    crate::bgio::submit("process account", move || {
        let u = resolve(pid, create);
        {
            let mut c = cache();
            c.seq += 1;
            let seq = c.seq;
            c.map.insert((pid, create), Entry { user: u, seq });
        }
        crate::ui::icons::poke();
    });
    User::Pending
}

fn ft(f: FILETIME) -> i64 {
    ((f.dwHighDateTime as i64) << 32) | f.dwLowDateTime as i64
}

/// Blocking: open the process, check it is still the same one, read its token user.
pub(crate) fn resolve(pid: u32, create: i64) -> User {
    if pid <= 4 {
        return User::Unavailable("system process");
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return User::Unavailable("access denied") };
        let u = of_handle(h, create);
        let _ = CloseHandle(h);
        u
    }
}

unsafe fn of_handle(h: HANDLE, create: i64) -> User {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_err() {
        return User::Unavailable("process details not readable");
    }
    // the pid may have been reused between the hover and now
    if ft(c) != create {
        return User::Unavailable("process has exited");
    }
    let mut tok = HANDLE::default();
    if OpenProcessToken(h, TOKEN_QUERY, &mut tok).is_err() {
        return User::Unavailable("access denied (another user's or protected process)");
    }
    let out = token_user(tok);
    let _ = CloseHandle(tok);
    out
}

unsafe fn token_user(tok: HANDLE) -> User {
    let mut need = 0u32;
    let _ = GetTokenInformation(tok, TokenUser, None, 0, &mut need);
    if need == 0 || need > 4096 {
        return User::Unavailable("account not readable");
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    if GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr() as *mut _), need, &mut need).is_err() {
        return User::Unavailable("account not readable");
    }
    let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
    let (mut nl, mut dl) = (256u32, 256u32);
    let (mut name, mut dom) = (vec![0u16; nl as usize], vec![0u16; dl as usize]);
    let mut kind = SID_NAME_USE::default();
    if LookupAccountSidW(None, sid, Some(PWSTR(name.as_mut_ptr())), &mut nl, Some(PWSTR(dom.as_mut_ptr())), &mut dl, &mut kind).is_err() {
        return User::Unavailable("account name not resolved");
    }
    let n = String::from_utf16_lossy(&name[..nl as usize]);
    let d = String::from_utf16_lossy(&dom[..dl as usize]);
    let full = if d.is_empty() { n } else { format!("{d}\\{n}") };
    let mut el = TOKEN_ELEVATION::default();
    let mut ret = 0u32;
    let elevated = GetTokenInformation(tok, TokenElevation, Some(&mut el as *mut _ as *mut _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut ret).ok().map(|_| el.TokenIsElevated != 0);
    User::Known { name: Arc::from(full), elevated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn own_create() -> i64 {
        use windows::Win32::System::Threading::GetCurrentProcess;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).unwrap() };
        ft(c)
    }

    #[test]
    fn own_account_is_found_and_a_reused_pid_is_refused() {
        let (pid, create) = (std::process::id(), own_create());
        match resolve(pid, create) {
            User::Known { name, .. } => assert!(!name.is_empty()),
            other => panic!("own process: {other:?}"),
        }
        // same pid, other create time: not the same process, no account is attributed to it
        assert_eq!(resolve(pid, create + 1), User::Unavailable("process has exited"));
        assert_eq!(resolve(4, 0), User::Unavailable("system process"));
    }

    #[test]
    fn lookup_never_blocks_and_fills_in_per_identity() {
        let (pid, create) = (std::process::id(), own_create());
        assert_eq!(lookup(pid, create), User::Pending, "first ask answers at once, with nothing");
        let t0 = Instant::now();
        loop {
            if matches!(lookup(pid, create), User::Known { .. }) {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "worker never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
        // another identity of the same pid has its own entry
        assert_eq!(lookup(pid, create + 7), User::Pending);
    }

    #[test]
    fn cache_is_bounded() {
        for i in 0..(KEEP as u32 * 3) {
            let _ = lookup(5000 + i, i as i64 + 1);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert!(cache().map.len() <= KEEP + MAX_PENDING + 1);
    }
}
