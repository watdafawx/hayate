//! Administrator-protected proof of an all-users take-over.
//!
//! The ownership records live in `startup.ini` under `%LOCALAPPDATA%\Hayate`,
//! which any process of the user can write, also an unelevated one. For a
//! per-user entry that is fine: whoever can forge the record can write the
//! HKCU value it points at anyway. For an all-users entry it is not: an
//! elevated Hayate (Restart as admin, `--startup-release` from an elevated
//! `install.ps1 -Uninstall`) would write an HKLM StartupApproved value chosen
//! by an unelevated process.
//!
//! So when an elevated Hayate takes over an all-users entry it ALSO writes the
//! record (written bytes + original bytes) into
//! `HKLM\SOFTWARE\Hayate\StartupOwnership`, a key it creates with a protected
//! access list: SYSTEM and Administrators full, Users read. Giving an all-users
//! entry back requires that copy to exist, to be in a key that only SYSTEM and
//! Administrators can change (owner and every write-granting entry checked on
//! the open handle), and to match the user-store record exactly. Anything else
//! means the record is not trusted: nothing is written, and the caller says so.
//!
//! An existing key with a wider access list is never "fixed" (no ACL is ever
//! widened or rewritten); it just never proves anything.

use std::ffi::c_void;

use windows::Win32::Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::*;
use windows::Win32::System::Registry::*;

use super::approved::{self, Locations};
use super::id::{EntryId, Scope};
use super::reg::{self, Key};
use crate::osutil::{pcw, wide};

/// Under the HKLM root of [`Locations`] (the real HKLM in production).
pub const KEY: &str = r"SOFTWARE\Hayate\StartupOwnership";
/// Owner Administrators; protected (nothing inherited); SYSTEM + Administrators full, Users read.
const SDDL: &str = "O:BAD:P(A;;KA;;;SY)(A;;KA;;;BA)(A;;KR;;;BU)";

/// Access bits that let a holder change, replace or re-permission the key.
const WRITE_BITS: u32 = 0x0002 // KEY_SET_VALUE
    | 0x0004 // KEY_CREATE_SUB_KEY
    | 0x0020 // KEY_CREATE_LINK
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE

#[cfg(test)]
thread_local! {
    /// test-only: treat the scratch proof key (made by the unelevated test user)
    /// as admin-protected, so the all-users round trip can be tested at all
    pub static TRUST_ANY_KEY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn trust_any() -> bool {
    #[cfg(test)]
    if TRUST_ANY_KEY.with(|t| t.get()) {
        return true;
    }
    false
}

fn is(sid: PSID, t: WELL_KNOWN_SID_TYPE) -> bool {
    unsafe { IsWellKnownSid(sid, t).as_bool() }
}

/// Only SYSTEM and Administrators (and the owner placeholders, which resolve to
/// the already checked owner) may hold write access.
fn trusted_writer(sid: PSID) -> bool {
    is(sid, WinLocalSystemSid) || is(sid, WinBuiltinAdministratorsSid) || is(sid, WinCreatorOwnerSid) || is(sid, WinCreatorOwnerRightsSid)
}

/// Does this self-relative security descriptor let ONLY SYSTEM and
/// Administrators change the object? `Err` = why not. Pure (no object opened).
pub fn sd_admin_only(sd: PSECURITY_DESCRIPTOR) -> Result<(), &'static str> {
    unsafe {
        let mut owner = PSID::default();
        let mut dflt = windows::core::BOOL(0);
        GetSecurityDescriptorOwner(sd, &mut owner, &mut dflt).map_err(|_| "its owner cannot be read")?;
        if owner.0.is_null() || !IsValidSid(owner).as_bool() || !(is(owner, WinLocalSystemSid) || is(owner, WinBuiltinAdministratorsSid)) {
            return Err("it is not owned by SYSTEM or Administrators");
        }
        let mut present = windows::core::BOOL(0);
        let mut acl: *mut ACL = std::ptr::null_mut();
        GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut dflt).map_err(|_| "its access list cannot be read")?;
        if !present.as_bool() || acl.is_null() {
            return Err("it has no access list (anyone may change it)");
        }
        for i in 0..(*acl).AceCount as u32 {
            let mut ace: *mut c_void = std::ptr::null_mut();
            GetAce(acl, i, &mut ace).map_err(|_| "its access list cannot be read")?;
            let h = &*(ace as *const ACE_HEADER);
            if h.AceFlags as u32 & INHERIT_ONLY_ACE.0 != 0 {
                continue; // applies to subkeys only; the proof values live in this key
            }
            if matches!(h.AceType, 1 | 6 | 10 | 12) {
                continue; // deny entries never grant anything
            }
            // the mask sits right after the header in every ACE type
            let mask = (*(ace as *const ACCESS_ALLOWED_ACE)).Mask;
            if mask & WRITE_BITS == 0 {
                continue;
            }
            // plain and callback allow entries carry the SID right after the mask; any other kind is not guessed at
            if h.AceType != 0 && h.AceType != 9 {
                return Err("its access list has an entry Hayate cannot check");
            }
            let sid = PSID(&(*(ace as *const ACCESS_ALLOWED_ACE)).SidStart as *const u32 as *mut c_void);
            if !IsValidSid(sid).as_bool() || !trusted_writer(sid) {
                return Err("someone other than SYSTEM or Administrators may change it");
            }
        }
        Ok(())
    }
}

/// Check the descriptor of the OPEN key (no re-open between check and use).
fn key_admin_only(k: &Key) -> Result<(), String> {
    if trust_any() {
        return Ok(());
    }
    let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut n = 0u32;
    let r = unsafe { RegGetKeySecurity(k.0, info, None, &mut n) };
    if r != ERROR_INSUFFICIENT_BUFFER && r != ERROR_SUCCESS {
        return Err(format!("its permissions cannot be read (Windows error {})", r.0));
    }
    let mut buf = vec![0u64; (n as usize).div_ceil(8).max(1)];
    let r = unsafe { RegGetKeySecurity(k.0, info, Some(PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut c_void)), &mut n) };
    if r != ERROR_SUCCESS {
        return Err(format!("its permissions cannot be read (Windows error {})", r.0));
    }
    sd_admin_only(PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut c_void)).map_err(str::to_string)
}

/// Value name of one entry (case folded: entry ids compare case-insensitively).
fn name(id: &EntryId) -> String {
    EntryId::new(id.kind, id.scope, id.view, id.key.to_lowercase()).encode()
}

fn encode(written: &[u8], original: Option<&[u8]>) -> Vec<u8> {
    let mut v = written.to_vec();
    v.extend_from_slice(original.unwrap_or(&[]));
    v
}

/// Open (creating with the protected access list when absent) for writing.
fn create(loc: &Locations) -> Result<Key, String> {
    let sam = KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY;
    if trust_any() {
        return reg::create(loc.hklm, KEY, sam).map_err(|e| e.text());
    }
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let s = wide(SDDL);
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(pcw(&s), SDDL_REVISION_1, &mut psd, None) }.map_err(|e| format!("cannot build its permissions: {e}"))?;
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd.0, bInheritHandle: false.into() };
    let w = wide(KEY);
    let mut k = HKEY::default();
    // an EXISTING key keeps its own descriptor (never rewritten); it is checked below like any other
    let r = unsafe { RegCreateKeyExW(loc.hklm, pcw(&w), None, None, REG_OPTION_NON_VOLATILE, sam, Some(&sa), &mut k, None) };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    if r != ERROR_SUCCESS {
        return Err(reg::RegErr(r.0).text());
    }
    Ok(Key(k))
}

/// Record an all-users take-over in the protected key. `Err` = the take-over
/// must not go ahead (it could never be given back).
pub fn save(loc: &Locations, id: &EntryId, written: &[u8], original: Option<&[u8]>) -> Result<(), String> {
    debug_assert_eq!(id.scope, Scope::AllUsers);
    let k = create(loc)?;
    key_admin_only(&k).map_err(|why| format!(r"HKLM\{KEY} is not protected ({why})"))?;
    reg::set_binary(&k, &name(id), &encode(written, original)).map_err(|e| e.text())?;
    match reg::get(&k, &name(id)) {
        Ok(Some((ty, b))) if ty == REG_BINARY.0 && b == encode(written, original) => Ok(()),
        _ => Err("it did not read back".into()),
    }
}

/// `Ok` only when the protected key holds exactly this record. `Err` = why the
/// user-store record is not trusted. Read-only.
pub fn check(loc: &Locations, id: &EntryId, written: &[u8], original: Option<&[u8]>) -> Result<(), String> {
    if !approved::canonical(written) || original.is_some_and(|b| !approved::canonical(b)) {
        return Err("its record holds a value Windows itself never writes".into());
    }
    let k = match reg::open(loc.hklm, KEY, KEY_READ | KEY_WOW64_64KEY) {
        Ok(k) => k,
        Err(e) if e.not_found() => return Err("no administrator-protected record of this take-over exists".into()),
        Err(e) => return Err(format!("the administrator-protected record cannot be read ({})", e.text())),
    };
    key_admin_only(&k).map_err(|why| format!(r"HKLM\{KEY} is not administrator-protected ({why})"))?;
    match reg::get(&k, &name(id)) {
        Ok(Some((ty, b))) if ty == REG_BINARY.0 && b == encode(written, original) => Ok(()),
        Ok(Some(_)) => Err("the administrator-protected record of this take-over says something else".into()),
        Ok(None) => Err("no administrator-protected record of this take-over exists".into()),
        Err(e) => Err(format!("the administrator-protected record cannot be read ({})", e.text())),
    }
}

/// Drop the protected copy once the take-over is over. Best effort: an
/// unelevated process cannot, and a stale copy proves nothing on its own (a
/// give-back still needs the live value to be exactly the bytes Hayate wrote).
pub fn forget(loc: &Locations, id: &EntryId) {
    if let Ok(k) = reg::open(loc.hklm, KEY, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY) {
        if key_admin_only(&k).is_ok() {
            let _ = reg::delete(&k, &name(id));
        }
    }
}

/// Only an elevated process can change the real protected key; an unelevated
/// one does not even try (tests use the scratch stand-in instead).
fn may_change() -> bool {
    trust_any() || crate::osutil::is_elevated()
}

/// Drop the copy of a record that is being dropped WITHOUT a give-back
/// (reconcile found the take-over never happened, or the entry was changed
/// outside Hayate since). Only from an elevated process and only when the copy
/// proves exactly this record, so a forged record never erases another
/// record's copy. In both cases the live value is no longer the bytes Hayate
/// wrote, so the copy could not give anything back any more. Never fails;
/// true = the copy was removed.
pub fn forget_proven(loc: &Locations, id: &EntryId, written: &[u8], original: Option<&[u8]>) -> bool {
    if !may_change() || check(loc, id, written, original).is_err() {
        return false;
    }
    forget(loc, id);
    check(loc, id, written, original).is_err()
}

/// `SOFTWARE\Hayate` under the HKLM root: removed by [`tidy`] only when empty.
const PARENT: &str = r"SOFTWARE\Hayate";

/// No values and no subkeys (unreadable = not empty).
fn empty(k: &Key) -> bool {
    let (mut subkeys, mut values) = (u32::MAX, u32::MAX);
    let r = unsafe { RegQueryInfoKeyW(k.0, None, None, None, Some(&mut subkeys), None, None, Some(&mut values), None, None, None, None) };
    r == ERROR_SUCCESS && subkeys == 0 && values == 0
}

fn delete_key(root: HKEY, path: &str) -> bool {
    let w = wide(path);
    unsafe { RegDeleteKeyExW(root, pcw(&w), KEY_WOW64_64KEY.0, None) == ERROR_SUCCESS }
}

/// After an uninstall gave everything back: remove the protected key once it
/// holds no copy at all (another user's take-over keeps it), and then
/// `SOFTWARE\Hayate` if that is empty too. Elevated only; an unreadable,
/// non-empty or not admin-protected key is kept. Never fails; true = the
/// StartupOwnership key was removed.
pub fn tidy(loc: &Locations) -> bool {
    if !may_change() {
        return false;
    }
    {
        let Ok(k) = reg::open(loc.hklm, KEY, KEY_READ | KEY_WOW64_64KEY) else { return false };
        if key_admin_only(&k).is_err() || !empty(&k) {
            return false;
        }
    }
    if !delete_key(loc.hklm, KEY) {
        return false;
    }
    let parent_empty = reg::open(loc.hklm, PARENT, KEY_READ | KEY_WOW64_64KEY).is_ok_and(|p| empty(&p));
    if parent_empty {
        delete_key(loc.hklm, PARENT);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::id::{Kind, View};
    use crate::startup::reg::scratch::Scratch;

    fn sd_ok(sddl: &str) -> Result<(), &'static str> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let s = wide(sddl);
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(pcw(&s), SDDL_REVISION_1, &mut psd, None) }.expect(sddl);
        let r = sd_admin_only(psd);
        unsafe {
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
        r
    }

    #[test]
    fn only_an_admin_owned_admin_writable_descriptor_proves_anything() {
        assert_eq!(sd_ok(SDDL), Ok(()), "the descriptor Hayate creates the key with");
        assert_eq!(sd_ok("O:SYD:P(A;;KA;;;SY)(A;;KA;;;BA)(A;;KR;;;BU)(A;OICIIO;KA;;;CO)"), Ok(()));
        // the default HKLM\SOFTWARE shape (inherited CREATOR OWNER, read for users and app packages)
        assert_eq!(sd_ok("O:BAD:AI(A;ID;KA;;;BA)(A;ID;KA;;;SY)(A;CIIOID;KA;;;CO)(A;ID;KR;;;BU)(A;ID;KR;;;AC)"), Ok(()));
        assert_eq!(sd_ok("O:BAD:(D;;KA;;;BU)(A;;KA;;;BA)"), Ok(()), "a deny entry grants nothing");
        assert_eq!(sd_ok("O:BAD:P(A;;KA;;;BA)(A;OICIIO;KA;;;BU)"), Ok(()), "inherit-only: subkeys, not these values");

        assert!(sd_ok("O:BUD:P(A;;KA;;;SY)(A;;KA;;;BA)").is_err(), "owned by Users");
        assert!(sd_ok("O:WDD:P(A;;KA;;;BA)").is_err(), "owned by Everyone");
        assert!(sd_ok("O:BAD:P(A;;KA;;;BA)(A;;KA;;;BU)").is_err(), "Users full");
        assert!(sd_ok("O:BAD:P(A;;KA;;;BA)(A;;0x2;;;WD)").is_err(), "Everyone may set values");
        assert!(sd_ok("O:BAD:P(A;;KA;;;BA)(A;;WD;;;AU)").is_err(), "Authenticated Users may rewrite the permissions");
        assert!(sd_ok("O:BAD:P(A;;KA;;;BA)(A;;SD;;;IU)").is_err(), "Interactive may delete it");
        assert!(sd_ok("O:BAD:P(A;;KA;;;BA)(A;;GA;;;BU)").is_err(), "generic all");
        assert!(sd_ok("O:BA").is_err(), "no access list at all = anyone");
        assert!(sd_ok("O:BAD:NO_ACCESS_CONTROL").is_err(), "null access list = anyone");
    }

    fn id() -> EntryId {
        EntryId::new(Kind::Run, Scope::AllUsers, View::Native, "HayateStartupFixture Proof")
    }

    fn loc(s: &Scratch) -> Locations {
        Locations { hkcu: s.user.0, hklm: s.machine.0, user_dir: Default::default(), common_dir: Default::default() }
    }

    #[test]
    fn round_trip_binds_written_and_original_exactly() {
        let s = Scratch::new("proof-rt");
        let l = loc(&s);
        TRUST_ANY_KEY.with(|t| t.set(true));
        let w = approved::disabled_bytes(42);
        let o = approved::enabled_bytes();
        assert!(check(&l, &id(), &w, Some(&o)).unwrap_err().contains("no administrator-protected record"));
        save(&l, &id(), &w, Some(&o)).unwrap();
        assert_eq!(check(&l, &id(), &w, Some(&o)), Ok(()));
        // the key's case does not matter, the bytes do
        let upper = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, "HAYATESTARTUPFIXTURE PROOF");
        assert_eq!(check(&l, &upper, &w, Some(&o)), Ok(()));
        assert!(check(&l, &id(), &w, None).is_err(), "other original");
        assert!(check(&l, &id(), &approved::disabled_bytes(43), Some(&o)).is_err(), "other written");
        assert!(check(&l, &EntryId::new(Kind::Run, Scope::AllUsers, View::Wow32, "HayateStartupFixture Proof"), &w, Some(&o)).is_err(), "other entry");
        forget(&l, &id());
        assert!(check(&l, &id(), &w, Some(&o)).is_err());
        TRUST_ANY_KEY.with(|t| t.set(false));
    }

    /// A key an unelevated process made (it would own it) never proves anything,
    /// even when it holds exactly the right bytes.
    #[test]
    fn a_key_the_user_made_proves_nothing() {
        if crate::osutil::is_elevated() {
            return; // an elevated test run creates keys owned by Administrators
        }
        let s = Scratch::new("proof-forged");
        let l = loc(&s);
        let w = approved::enabled_bytes();
        let o = approved::disabled_bytes(7);
        let k = reg::create(l.hklm, KEY, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
        reg::set_binary(&k, &name(&id()), &encode(&w, Some(&o))).unwrap();
        let err = check(&l, &id(), &w, Some(&o)).unwrap_err();
        assert!(err.contains("not administrator-protected"), "{err}");
        // and Hayate never writes a record into it either
        assert!(save(&l, &id(), &w, Some(&o)).is_err());
    }

    fn has_value(l: &Locations, id: &EntryId) -> bool {
        reg::open(l.hklm, KEY, KEY_READ | KEY_WOW64_64KEY).is_ok_and(|k| matches!(reg::get(&k, &name(id)), Ok(Some(_))))
    }

    #[test]
    fn forget_proven_removes_only_a_matching_copy() {
        let s = Scratch::new("proof-forget");
        let l = loc(&s);
        TRUST_ANY_KEY.with(|t| t.set(true));
        let w = approved::disabled_bytes(5);
        save(&l, &id(), &w, None).unwrap();
        assert!(!forget_proven(&l, &id(), &approved::disabled_bytes(6), None), "other written bytes");
        assert!(!forget_proven(&l, &id(), &w, Some(&approved::enabled_bytes())), "other original");
        assert!(has_value(&l, &id()));
        assert!(forget_proven(&l, &id(), &w, None));
        assert!(!has_value(&l, &id()));
        assert!(!forget_proven(&l, &id(), &w, None), "nothing left to remove");
        TRUST_ANY_KEY.with(|t| t.set(false));
    }

    /// Unelevated (no stand-in trust): nothing is removed, whatever the key holds.
    #[test]
    fn an_unelevated_process_never_removes_anything() {
        if crate::osutil::is_elevated() {
            return;
        }
        let s = Scratch::new("proof-unelev");
        let l = loc(&s);
        let w = approved::disabled_bytes(5);
        TRUST_ANY_KEY.with(|t| t.set(true));
        save(&l, &id(), &w, None).unwrap();
        TRUST_ANY_KEY.with(|t| t.set(false));
        assert!(!forget_proven(&l, &id(), &w, None));
        assert!(has_value(&l, &id()));
        forget(&l, &id()); // the plain forget also needs an admin-protected key
        assert!(has_value(&l, &id()));
        let k = reg::open(l.hklm, KEY, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY).unwrap();
        reg::delete(&k, &name(&id())).unwrap();
        drop(k);
        assert!(!tidy(&l), "an empty key is kept by an unelevated process");
        assert!(reg::open(l.hklm, KEY, KEY_READ).is_ok());
    }

    #[test]
    fn tidy_removes_only_an_empty_key_and_an_empty_parent() {
        let s = Scratch::new("proof-tidy");
        let l = loc(&s);
        TRUST_ANY_KEY.with(|t| t.set(true));
        assert!(!tidy(&l), "no key: nothing to do");
        let w = approved::disabled_bytes(5);
        save(&l, &id(), &w, None).unwrap();
        assert!(!tidy(&l), "a copy is still held (another take-over)");
        assert!(has_value(&l, &id()));
        forget(&l, &id());
        // something else lives in SOFTWARE\Hayate: the parent stays
        let p = reg::open(l.hklm, PARENT, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY).unwrap();
        reg::set_binary(&p, "other", &[1]).unwrap();
        drop(p);
        assert!(tidy(&l));
        assert!(matches!(reg::open(l.hklm, KEY, KEY_READ), Err(e) if e.not_found()));
        assert!(reg::open(l.hklm, PARENT, KEY_READ).is_ok(), "a non-empty parent is kept");
        // a subkey keeps the protected key too
        let k = create(&l).unwrap();
        drop(k);
        reg::create(l.hklm, &format!(r"{KEY}\sub"), KEY_READ | KEY_WRITE).unwrap();
        assert!(!tidy(&l));
        assert!(reg::open(l.hklm, KEY, KEY_READ).is_ok());
        TRUST_ANY_KEY.with(|t| t.set(false));
    }

    /// Owner, group and access list of the OPEN key as SDDL (for the record of the real-HKLM run).
    fn key_sddl(k: &Key) -> String {
        use windows::core::PWSTR;
        use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
        let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut n = 0u32;
        unsafe {
            let _ = RegGetKeySecurity(k.0, info, None, &mut n);
            let mut buf = vec![0u64; (n as usize).div_ceil(8).max(1)];
            assert_eq!(RegGetKeySecurity(k.0, info, Some(PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut c_void)), &mut n), ERROR_SUCCESS);
            let mut out = PWSTR::null();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut c_void), SDDL_REVISION_1, info, &mut out, None).unwrap();
            let t = out.to_string().unwrap();
            let _ = LocalFree(Some(HLOCAL(out.0 as *mut c_void)));
            t
        }
    }

    /// One-time proof against the REAL HKLM (Job B, 2026-11-18, rig only).
    /// Never runs by default: needs `--ignored`, `HAYATE_REAL_HKLM_PROOF=1` and an
    /// elevated process. Touches only two uniquely named `zz-hayateproof-*`
    /// values under HKLM StartupApproved\Run (no Run value, no real entry), their
    /// copies in the StartupOwnership key, and that key; it removes all of them,
    /// and the key (and SOFTWARE\Hayate) only when this run created them.
    #[test]
    #[ignore]
    fn real_hklm_proof_round_trip() {
        use crate::startup::adopt::{self, Released, WRITES};
        use crate::startup::model::{App, Own, OwnState};
        use crate::startup::store::Store;
        if crate::osutil::env_var("REAL_HKLM_PROOF").as_deref() != Ok("1") {
            eprintln!("skipped: set HAYATE_REAL_HKLM_PROOF=1 (elevated, rig only)");
            return;
        }
        assert!(crate::osutil::is_elevated(), "must run elevated");
        let loc = Locations::system();
        let tag = format!("{}-{}", std::process::id(), approved::filetime_now() % 1_000_000_007);
        let fake = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, format!("zz-hayateproof-{tag}"));
        let forged = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, format!("zz-hayateproof-{tag}-forged"));
        let approved_path = Locations::approved_path(&fake);
        let exists = |p: &str| reg::open(loc.hklm, p, KEY_READ | KEY_WOW64_64KEY).is_ok();
        let (had_parent, had_key, had_approved) = (exists(PARENT), exists(KEY), exists(&approved_path));
        println!("before: HKLM\\{PARENT} exists={had_parent}, HKLM\\{KEY} exists={had_key}, HKLM\\{approved_path} exists={had_approved}");
        println!("fake values: {} and {}", fake.key, forged.key);

        struct Cleanup {
            fake: EntryId,
            forged: EntryId,
            had_parent: bool,
            had_key: bool,
            had_approved: bool,
            approved_path: String,
            dir: std::path::PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
                let loc = Locations::system();
                for id in [&self.fake, &self.forged] {
                    let _ = approved::write(&loc, id, None);
                    forget(&loc, id);
                }
                if !self.had_key {
                    if let Ok(k) = reg::open(loc.hklm, KEY, KEY_READ | KEY_WOW64_64KEY) {
                        let e = empty(&k);
                        drop(k);
                        if e {
                            delete_key(loc.hklm, KEY);
                        }
                    }
                }
                if !self.had_parent {
                    if let Ok(k) = reg::open(loc.hklm, PARENT, KEY_READ | KEY_WOW64_64KEY) {
                        let e = empty(&k);
                        drop(k);
                        if e {
                            delete_key(loc.hklm, PARENT);
                        }
                    }
                }
                if !self.had_approved {
                    if let Ok(k) = reg::open(loc.hklm, &self.approved_path, KEY_READ | KEY_WOW64_64KEY) {
                        let e = empty(&k);
                        drop(k);
                        if e {
                            delete_key(loc.hklm, &self.approved_path);
                        }
                    }
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("hayateproof-{tag}"));
        let _guard = Cleanup { fake: fake.clone(), forged: forged.clone(), had_parent, had_key, had_approved, approved_path: approved_path.clone(), dir: dir.clone() };
        let writes = || WRITES.with(|w| std::mem::take(&mut *w.borrow_mut()));
        let live = |id: &EntryId| approved::read(&loc, id).unwrap().approved;

        // 1. the fake entry, "disabled by Hayate", and its protected copy (adopt step 3b)
        let now = approved::filetime_now();
        let written = approved::disabled_bytes(now);
        let original = approved::enabled_bytes();
        approved::write(&loc, &fake, Some(&written)).unwrap();
        save(&loc, &fake, &written, Some(&original)).expect("elevated create + save of the protected copy");
        let k = reg::open(loc.hklm, KEY, KEY_READ | KEY_WOW64_64KEY).unwrap();
        let sddl = key_sddl(&k);
        println!("1. HKLM\\{KEY} SDDL (from RegGetKeySecurity): {sddl}");
        if !had_key {
            assert!(sddl.starts_with("O:BA"), "owner Administrators: {sddl}");
            assert!(sddl.ends_with("D:P(A;;KA;;;SY)(A;;KA;;;BA)(A;;KR;;;BU)"), "protected, SYSTEM+Administrators full, Users read: {sddl}");
        }
        let ps = std::process::Command::new("pwsh")
            .stdin(std::process::Stdio::null())
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("(Get-Acl 'HKLM:\\{KEY}').Sddl; (Get-Acl 'HKLM:\\{KEY}').Access | ForEach-Object {{ '{{0}} {{1}} {{2}}' -f $_.IdentityReference,$_.RegistryRights,$_.AccessControlType }}")])
            .output()
            .unwrap();
        println!("1. Get-Acl ({}):\n{}\n{}", ps.status, String::from_utf8_lossy(&ps.stdout).trim(), String::from_utf8_lossy(&ps.stderr).trim());
        if had_parent {
            println!("1. HKLM\\{PARENT} existed before");
        } else {
            println!("1. HKLM\\{PARENT} SDDL (created by this run): {}", key_sddl(&reg::open(loc.hklm, PARENT, KEY_READ | KEY_WOW64_64KEY).unwrap()));
        }
        // 2. the check passes on the real key
        assert_eq!(key_admin_only(&k), Ok(()));
        drop(k);
        assert_eq!(check(&loc, &fake, &written, Some(&original)), Ok(()));
        println!("2. check: Ok");

        // 3. forged records in a user-writable store: the skip path writes nothing
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::new(dir.join("startup.ini"));
        store.load();
        let add = |source: &EntryId, state: OwnState, w: Vec<u8>, o: Option<Vec<u8>>| -> String {
            let mut id = String::new();
            store
                .update_sync(|c| {
                    let fid = c.fresh_id();
                    let mut a = App::new(fid.clone(), format!("zz-hayateproof {fid}"), r"C:\nope\zz-hayateproof.exe".into());
                    a.own = Some(Own { source: source.clone(), state, original: o.clone(), written: w.clone(), raw_at_adopt: String::new() });
                    id = a.id.clone();
                    c.apps.push(a);
                })
                .unwrap();
            id
        };
        let forged_written = approved::disabled_bytes(now + 1);
        approved::write(&loc, &forged, Some(&forged_written)).unwrap();
        writes();
        for (what, src, w, o) in [
            ("record whose original differs from the copy", &fake, written.clone(), Some(approved::disabled_bytes(1))),
            ("record with no copy at all", &forged, forged_written.clone(), Some(original.clone())),
        ] {
            let before = live(src);
            let app = add(src, OwnState::Owned, w, o);
            let rel = adopt::release(&loc, &store, &app).unwrap();
            assert!(matches!(rel, Released::ChangedSince(ref why) if why.contains("Task Manager")), "{rel:?}");
            assert!(writes().is_empty(), "{what}: no StartupApproved write attempted");
            assert_eq!(live(src), before, "{what}: live value unchanged");
            assert!(store.snapshot().cfg.apps.iter().all(|a| a.own.is_none()), "{what}: record dropped");
            println!("3. forged {what}: skipped, nothing written ({rel:?})");
        }
        assert_eq!(check(&loc, &fake, &written, Some(&original)), Ok(()), "the real copy survives the forged give-backs");

        // 4. a legit give-back of the fake value: original back, copy removed
        let app = add(&fake, OwnState::Owned, written.clone(), Some(original.clone()));
        assert_eq!(adopt::release(&loc, &store, &app).unwrap(), Released::Restored);
        assert_eq!(writes(), vec![fake.clone()]);
        assert_eq!(live(&fake), Some(original.clone()), "the recorded original is back");
        assert!(check(&loc, &fake, &written, Some(&original)).is_err(), "copy removed after the proven restore");
        println!("4. legit give-back: Restored, copy removed");

        // 5. changed outside Hayate: record dropped, its own copy removed (new in Job A)
        let w2 = approved::disabled_bytes(now + 2);
        save(&loc, &fake, &w2, Some(&original)).unwrap();
        let app = add(&fake, OwnState::Owned, w2.clone(), Some(original.clone()));
        assert!(matches!(adopt::release(&loc, &store, &app).unwrap(), Released::ChangedSince(_)));
        assert!(writes().is_empty());
        assert!(check(&loc, &fake, &w2, Some(&original)).is_err(), "copy removed with the dropped record");
        println!("5. changed-since give-back: nothing written, copy removed");

        // 6. reconcile drops a take-over that never disabled anything, with its copy
        let w3 = approved::disabled_bytes(now + 3);
        save(&loc, &fake, &w3, Some(&original)).unwrap();
        let pid = add(&fake, OwnState::Pending, w3.clone(), Some(original.clone()));
        adopt::reconcile(&loc, &store);
        assert!(store.snapshot().cfg.app(&pid).is_none(), "the pending take-over is dropped");
        assert!(writes().is_empty());
        assert!(check(&loc, &fake, &w3, Some(&original)).is_err(), "copy removed with the dropped pending record");
        println!("6. reconcile drop: nothing written, copy removed");

        // 7. cleanup: the fake values, then the now empty key (and parent) through tidy
        for id in [&fake, &forged] {
            approved::write(&loc, id, None).unwrap();
            assert_eq!(live(id), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
        if !had_key && !had_parent {
            assert!(tidy(&loc), "the empty key is removed");
            assert!(!exists(KEY) && !exists(PARENT));
        }
        println!("7. after: HKLM\\{PARENT} exists={}, HKLM\\{KEY} exists={}, fake values gone", exists(PARENT), exists(KEY));
    }

    #[test]
    fn a_non_canonical_record_is_never_proven() {
        let s = Scratch::new("proof-canon");
        let l = loc(&s);
        assert!(check(&l, &id(), &[0xff], None).unwrap_err().contains("never writes"));
    }
}
