//! Where startup entries live and what Windows says about each of them.
//!
//! "Enabled" is not a property of the Run value: Windows (Task Manager,
//! Settings > Apps > Startup) keeps it in a separate `StartupApproved` value of
//! the same name: 12 bytes, a flag DWORD (low bit set = disabled) and, when
//! disabled, the FILETIME it happened. A missing value means enabled.
//!
//! Nothing here runs a program. Reads open keys read-only; the only writes are
//! one StartupApproved value (set or delete) per call, through [`write`].

use std::path::PathBuf;

use windows::Win32::System::Registry::*;

use super::id::{EntryId, Kind, Scope, View};
use super::reg::{self, RegErr};
use windows::Win32::Foundation::ERROR_INVALID_DATA;

pub const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
pub const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

/// Where to look. Production = the real hives and Known Folders; tests put
/// every part somewhere disposable.
#[derive(Clone)]
pub struct Locations {
    pub hkcu: HKEY,
    pub hklm: HKEY,
    pub user_dir: PathBuf,
    pub common_dir: PathBuf,
}

// HKEY is a plain handle value; the roots are predefined keys or handles a test keeps open.
unsafe impl Send for Locations {}
unsafe impl Sync for Locations {}

impl Locations {
    /// The real machine. Folder paths come from the Known Folder API (not from
    /// %APPDATA% / %ProgramData%, which a process environment can bend).
    pub fn system() -> Locations {
        Locations {
            hkcu: HKEY_CURRENT_USER,
            hklm: HKEY_LOCAL_MACHINE,
            user_dir: crate::setup::startup_folder(false).unwrap_or_default(),
            common_dir: crate::setup::startup_folder(true).unwrap_or_default(),
        }
    }

    pub fn root(&self, scope: Scope) -> HKEY {
        match scope {
            Scope::User => self.hkcu,
            Scope::AllUsers => self.hklm,
        }
    }

    pub fn dir(&self, scope: Scope) -> &PathBuf {
        match scope {
            Scope::User => &self.user_dir,
            Scope::AllUsers => &self.common_dir,
        }
    }

    /// Run key path of an entry's view.
    pub fn run_path(id: &EntryId) -> &'static str {
        match id.view {
            View::Native => RUN,
            View::Wow32 => RUN32,
        }
    }

    /// StartupApproved subkey of an entry (under `APPROVED`).
    pub fn approved_sub(id: &EntryId) -> &'static str {
        match (id.kind, id.view) {
            (Kind::Folder, _) => "StartupFolder",
            (Kind::Run, View::Native) => "Run",
            (Kind::Run, View::Wow32) => "Run32",
        }
    }

    pub fn approved_path(id: &EntryId) -> String {
        format!(r"{APPROVED}\{}", Self::approved_sub(id))
    }
}

/// What the 12 bytes say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Enabled,
    Disabled,
    /// a value that is not the documented shape: shown, never rewritten silently
    Unknown,
}

/// A missing value means "enabled": that is how Windows treats an entry nobody
/// switched off (Task Manager only creates the value when it disables or
/// re-enables an entry; checked on a real machine, Microsoft documents none of
/// this). Only the shapes in [`canonical`] are understood; every other value,
/// whatever its length, is [`State::Unknown`] and is never written over.
pub fn state_of(raw: Option<&[u8]>) -> State {
    match raw {
        None => State::Enabled,
        Some(b) if canonical(b) => {
            if b[0] & 1 == 1 {
                State::Disabled
            } else {
                State::Enabled
            }
        }
        Some(_) => State::Unknown,
    }
}

/// The value Task Manager writes when it disables an entry: flag 3 + FILETIME.
pub fn disabled_bytes(filetime: u64) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 3;
    b[4..12].copy_from_slice(&filetime.to_le_bytes());
    b
}

/// The value Task Manager writes when it enables an entry again.
pub fn enabled_bytes() -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 2;
    b
}

/// Now as a FILETIME.
pub fn filetime_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() / 100).unwrap_or(0) as u64 + 116_444_736_000_000_000
}

/// Only values of a shape Windows itself writes may be recorded or restored
/// (a hand-edited config must not turn this into an arbitrary-bytes writer).
pub fn canonical(b: &[u8]) -> bool {
    b.len() == 12 && matches!(b[0], 2 | 3 | 6 | 7) && b[1..4] == [0, 0, 0] && (b[0] & 1 == 1 || b[4..12] == [0; 8])
}

/// What one entry looks like in the registry / on disk right now.
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    /// the Run value or the folder file exists
    pub present: bool,
    /// a StartupApproved value exists in a registry type other than REG_BINARY
    /// (either layer): its meaning is unknown, so the state is Unknown
    pub wrong_type: bool,
    /// StartupApproved value of the entry's own scope (None = no value)
    pub approved: Option<Vec<u8>>,
    /// all-users entry only: the current user's own StartupApproved value
    /// (Windows consults it too; Hayate never writes it for an all-users entry)
    pub user_override: Option<Vec<u8>>,
}

impl Live {
    #[cfg(test)]
    pub fn new(present: bool, approved: Option<Vec<u8>>, user_override: Option<Vec<u8>>) -> Live {
        Live { present, wrong_type: false, approved, user_override }
    }

    /// The state could not be read: shown as Unknown, never as enabled.
    pub fn unreadable() -> Live {
        Live { present: true, wrong_type: true, approved: None, user_override: None }
    }

    /// The state Windows applies for this user. When the per-user layer and the
    /// all-users layer say different things, which one wins is not documented:
    /// the answer is Unknown, never a guess.
    pub fn effective(&self) -> State {
        if self.wrong_type {
            return State::Unknown;
        }
        let own = state_of(self.approved.as_deref());
        match &self.user_override {
            None => own,
            Some(o) => {
                let user = state_of(Some(o));
                if own == State::Unknown || user == State::Unknown || own != user {
                    State::Unknown
                } else {
                    own
                }
            }
        }
    }
}

/// (bytes, the value is not REG_BINARY)
fn approved_read(loc: &Locations, scope: Scope, id: &EntryId) -> Result<(Option<Vec<u8>>, bool), RegErr> {
    let key = match reg::open(loc.root(scope), &Locations::approved_path(id), KEY_READ | KEY_WOW64_64KEY) {
        Ok(k) => k,
        Err(e) if e.not_found() => return Ok((None, false)),
        Err(e) => return Err(e),
    };
    Ok(match reg::get(&key, &id.key)? {
        Some((ty, b)) => (Some(b), ty != REG_BINARY.0),
        None => (None, false),
    })
}

/// Read the live state of `id`. Read-only.
pub fn read(loc: &Locations, id: &EntryId) -> Result<Live, RegErr> {
    let present = match id.kind {
        Kind::Run => match reg::open(loc.root(id.scope), Locations::run_path(id), KEY_READ | KEY_WOW64_64KEY) {
            Ok(k) => reg::get(&k, &id.key)?.is_some(),
            Err(e) if e.not_found() => false,
            Err(e) => return Err(e),
        },
        Kind::Folder => {
            let d = loc.dir(id.scope);
            !d.as_os_str().is_empty() && d.join(&id.key).is_file()
        }
    };
    let (approved, t1) = approved_read(loc, id.scope, id)?;
    let (user_override, t2) = if id.scope == Scope::AllUsers { approved_read(loc, Scope::User, id)? } else { (None, false) };
    Ok(Live { present, wrong_type: t1 || t2, approved, user_override })
}

/// Can the current process write this entry's StartupApproved value? Opens the
/// key for write (creating nothing) so a standard user learns "needs
/// administrator" BEFORE anything is saved. Never changes an ACL.
pub fn writable(loc: &Locations, id: &EntryId) -> Result<(), RegErr> {
    let path = Locations::approved_path(id);
    match reg::open(loc.root(id.scope), &path, KEY_SET_VALUE | KEY_WOW64_64KEY) {
        Ok(_) => Ok(()),
        Err(e) if e.not_found() => {
            // the subkey does not exist yet: the write will create it, so the
            // nearest existing ancestor must allow creating a subkey
            let mut p = path.as_str();
            loop {
                p = p.rsplit_once('\\').map(|(head, _)| head).unwrap_or("");
                match reg::open(loc.root(id.scope), p, KEY_CREATE_SUB_KEY | KEY_WOW64_64KEY) {
                    Ok(_) => return Ok(()),
                    Err(e2) if e2.not_found() && !p.is_empty() => continue,
                    Err(e2) => return Err(e2),
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Set (`Some`) or delete (`None`) the entry's own-scope StartupApproved value.
///
/// Only a known REG_BINARY shape is ever written ([`canonical`]) and only over a
/// value that is absent or itself a known REG_BINARY shape: a value of another
/// type or an undocumented shape is refused (`INVALID_DATA`), not replaced.
pub fn write(loc: &Locations, id: &EntryId, value: Option<&[u8]>) -> Result<(), RegErr> {
    let path = Locations::approved_path(id);
    if value.is_some_and(|b| !canonical(b)) {
        return Err(RegErr(ERROR_INVALID_DATA.0));
    }
    let existing_ok = |k: &reg::Key| -> Result<(), RegErr> {
        match reg::get(k, &id.key)? {
            Some((ty, b)) if ty != REG_BINARY.0 || !canonical(&b) => Err(RegErr(ERROR_INVALID_DATA.0)),
            _ => Ok(()),
        }
    };
    match value {
        Some(b) => {
            let k = reg::create(loc.root(id.scope), &path, KEY_SET_VALUE | KEY_QUERY_VALUE | KEY_WOW64_64KEY)?;
            existing_ok(&k)?;
            reg::set_binary(&k, &id.key, b)
        }
        None => {
            let k = match reg::open(loc.root(id.scope), &path, KEY_SET_VALUE | KEY_QUERY_VALUE | KEY_WOW64_64KEY) {
                Ok(k) => k,
                Err(e) if e.not_found() => return Ok(()),
                Err(e) => return Err(e),
            };
            existing_ok(&k)?;
            reg::delete(&k, &id.key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::reg::scratch::Scratch;

    pub fn loc_for(s: &Scratch, dir: &std::path::Path) -> Locations {
        Locations { hkcu: s.user.0, hklm: s.machine.0, user_dir: dir.join("user"), common_dir: dir.join("common") }
    }

    #[test]
    fn states_and_canonical_shapes() {
        assert_eq!(state_of(None), State::Enabled);
        assert_eq!(state_of(Some(&enabled_bytes())), State::Enabled);
        assert_eq!(state_of(Some(&disabled_bytes(1))), State::Disabled);
        assert_eq!(state_of(Some(&[6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])), State::Enabled);
        assert_eq!(state_of(Some(&[7, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0])), State::Disabled);
        assert_eq!(state_of(Some(&[])), State::Unknown);
        assert_eq!(state_of(Some(&[1, 2])), State::Unknown);
        assert!(canonical(&enabled_bytes()) && canonical(&disabled_bytes(123)));
        assert!(!canonical(&[3, 0, 0, 0]) && !canonical(&[9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]) && !canonical(&[2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        // an "enabled" value carrying a time is not the shape Windows writes
        assert!(!canonical(&[2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn write_read_delete_on_scratch_keys() {
        let s = Scratch::new("approved");
        let tmp = std::env::temp_dir();
        let loc = loc_for(&s, &tmp);
        let id = EntryId::new(Kind::Run, Scope::User, View::Native, "HayateStartupFixture A");
        // the Run value, written by the test the way an installer would
        let run = reg::create(loc.hkcu, RUN, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
        unsafe {
            let n = crate::osutil::wide("HayateStartupFixture A");
            let v: Vec<u8> = crate::osutil::wide(r"C:\nope\fixture.exe").iter().flat_map(|c| c.to_le_bytes()).collect();
            assert_eq!(RegSetValueExW(run.0, crate::osutil::pcw(&n), None, REG_SZ, Some(&v)), windows::Win32::Foundation::ERROR_SUCCESS);
        }
        let live = read(&loc, &id).unwrap();
        assert!(live.present && live.approved.is_none());
        assert_eq!(live.effective(), State::Enabled);
        let dis = disabled_bytes(filetime_now());
        write(&loc, &id, Some(&dis)).unwrap();
        let live = read(&loc, &id).unwrap();
        assert_eq!(live.approved.as_deref(), Some(dis.as_slice()));
        assert_eq!(live.effective(), State::Disabled);
        write(&loc, &id, None).unwrap();
        assert_eq!(read(&loc, &id).unwrap().approved, None);
        // deleting what is not there is not an error
        write(&loc, &id, None).unwrap();
        assert!(writable(&loc, &id).is_ok());
    }

    #[test]
    fn all_users_entry_reports_the_user_override() {
        let s = Scratch::new("override");
        let loc = loc_for(&s, &std::env::temp_dir());
        let id = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, "HayateStartupFixture B");
        write(&loc, &id, Some(&disabled_bytes(5))).unwrap(); // machine layer: disabled
        assert_eq!(read(&loc, &id).unwrap().effective(), State::Disabled);
        // the same name written for the current user (the user layer) wins
        let user_id = EntryId::new(Kind::Run, Scope::User, View::Native, "HayateStartupFixture B");
        write(&loc, &user_id, Some(&enabled_bytes())).unwrap();
        let live = read(&loc, &id).unwrap();
        assert_eq!(live.approved.as_deref(), Some(disabled_bytes(5).as_slice()));
        assert_eq!(live.user_override.as_deref(), Some(enabled_bytes().as_slice()));
        assert_eq!(live.effective(), State::Unknown, "layers that disagree have no documented winner: not guessed");
    }

    #[test]
    fn a_value_that_is_not_binary_is_unknown_never_enabled_or_disabled() {
        let s = Scratch::new("approved-type");
        let loc = loc_for(&s, &std::env::temp_dir());
        let id = EntryId::new(Kind::Run, Scope::User, View::Native, "HayateStartupFixture T");
        let k = reg::create(loc.hkcu, &Locations::approved_path(&id), KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
        let n = crate::osutil::wide(&id.key);
        // a future Windows could store a DWORD here; its meaning is not ours to guess
        unsafe {
            assert_eq!(RegSetValueExW(k.0, crate::osutil::pcw(&n), None, REG_DWORD, Some(&[1u8, 0, 0, 0])), windows::Win32::Foundation::ERROR_SUCCESS);
        }
        assert_eq!(read(&loc, &id).unwrap().effective(), State::Unknown);
        // twelve REG_BINARY-looking bytes stored as another type are still not a value Hayate may rewrite
        let twelve = disabled_bytes(5);
        unsafe {
            assert_eq!(RegSetValueExW(k.0, crate::osutil::pcw(&n), None, REG_QWORD, Some(&twelve[..8])), windows::Win32::Foundation::ERROR_SUCCESS);
        }
        assert_eq!(read(&loc, &id).unwrap().effective(), State::Unknown);
        assert!(write(&loc, &id, Some(&enabled_bytes())).is_err(), "never overwrite a value of an unknown type");
        assert!(write(&loc, &id, None).is_err(), "nor delete it");
    }

    #[test]
    fn undocumented_binary_shapes_are_unknown() {
        for b in [vec![3u8, 0, 0, 0], vec![3, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], vec![9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], vec![3u8; 64], vec![2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]] {
            assert_eq!(state_of(Some(&b)), State::Unknown, "{b:?}");
        }
        assert_eq!(state_of(None), State::Enabled, "no value = enabled (Windows default)");
    }

    #[test]
    fn folder_entry_presence_follows_the_file() {
        let s = Scratch::new("folder");
        let dir = std::env::temp_dir().join(format!("HayateStartupFixture-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("user")).unwrap();
        let loc = loc_for(&s, &dir);
        let id = EntryId::new(Kind::Folder, Scope::User, View::Native, "Fixture.lnk");
        assert!(!read(&loc, &id).unwrap().present);
        std::fs::write(dir.join("user").join("Fixture.lnk"), b"x").unwrap();
        assert!(read(&loc, &id).unwrap().present);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
