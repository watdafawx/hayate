//! Hayate's own "Start with Windows" shortcut, as far as the startup manager is
//! concerned.
//!
//! The logon run needs proof that THIS start came from Hayate's own autorun and
//! not from a manual `--resident`, an update or a restart. Hayate does not add a
//! second autorun. The existing Startup shortcut (`Hayate.lnk`) gets one private
//! argument ([`LOGON_FLAG`]) when, and only when, the user turns the startup
//! manager on and presses Save; turning it off takes the argument out again.
//! Other arguments on the shortcut are kept as they are. If the shortcut cannot
//! be changed the switch stays off (rollback), and the shortcut file is put back
//! to its exact earlier bytes if saving it failed half way.
//!
//! The shortcut is user-writable, so it is only ever parsed by a Hayate that is
//! provably not elevated (callers check [`crate::osutil::elevation`]).

use std::path::PathBuf;

use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, STGM_READ};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

use super::approved::{self, Locations, State};
use super::id::{EntryId, Kind, Scope, View};
use super::run::LOGON_FLAG;
use super::scan;
use crate::osutil::{pcw, wide};

pub const LINK_NAME: &str = crate::setup::logic::LINK_AUTO;

/// What the logon rules need to know about the shortcut.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnAutorun {
    pub exists: bool,
    /// Windows has it enabled (StartupApproved; a missing value is Windows' default: enabled)
    pub enabled: bool,
    /// StartupApproved holds a value of a shape Hayate does not recognise: neither on nor off as far as
    /// Hayate can tell (`enabled` is false then, but that is not "switched off in Windows")
    pub unknown: bool,
    /// it starts a Hayate exe
    pub is_hayate: bool,
    pub args: String,
}

impl OwnAutorun {
    fn missing() -> OwnAutorun {
        OwnAutorun { exists: false, enabled: false, unknown: false, is_hayate: false, args: String::new() }
    }

    pub fn has_resident(&self) -> bool {
        self.args.split_whitespace().any(|a| a == "--resident")
    }

    pub fn has_flag(&self) -> bool {
        self.args.split_whitespace().any(|a| a == LOGON_FLAG)
    }

    /// The conditions the logon run needs from the shortcut.
    pub fn ok(&self) -> bool {
        self.exists && self.enabled && self.is_hayate && self.has_resident() && self.has_flag()
    }
}

fn path(loc: &Locations) -> PathBuf {
    loc.dir(Scope::User).join(LINK_NAME)
}

fn id() -> EntryId {
    EntryId::new(Kind::Folder, Scope::User, View::Native, LINK_NAME)
}

/// The shortcut as the Startup tab's last scan saw it (no I/O here). `None` =
/// not known: the scan has not arrived, the user's Startup folder could not be
/// read, or the shortcut target was not read (an elevated Hayate lists shortcuts
/// by name only).
pub fn from_scan(entries: &[super::scan::Entry], problems: &[String], user_dir: &std::path::Path) -> Option<OwnAutorun> {
    let dir = user_dir.to_string_lossy();
    if problems.iter().any(|p| p.starts_with("User Startup folder") || (!dir.is_empty() && p.starts_with(dir.as_ref()))) {
        return None;
    }
    let me = id();
    match entries.iter().find(|e| e.id.kind == me.kind && e.id.scope == me.scope && e.id.key.eq_ignore_ascii_case(LINK_NAME)) {
        None => Some(OwnAutorun::missing()),
        Some(e) => {
            let t = e.target.as_ref()?;
            let st = e.state();
            Some(OwnAutorun { exists: true, enabled: st == State::Enabled, unknown: st == State::Unknown, is_hayate: e.is_hayate, args: t.args.clone() })
        }
    }
}

/// Why the startup list cannot run at sign-in, in the order the checks are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// this Hayate is elevated (or cannot tell): it does not read or edit the shortcut
    Unchecked,
    /// the shortcut could not be read
    Unreadable,
    Missing,
    NotHayate,
    /// StartupApproved holds a shape Hayate does not recognise (never overwritten, so nothing to offer)
    UnknownShape,
    /// Windows switched it off (a value Hayate recognises)
    Disabled,
    NoResident,
    NoFlag,
}

/// First thing that stops the logon run, or `None` when the shortcut is right.
pub fn gap(own: Option<&OwnAutorun>, elevated: Option<bool>) -> Option<Gap> {
    if elevated != Some(false) {
        return Some(Gap::Unchecked);
    }
    let Some(o) = own else { return Some(Gap::Unreadable) };
    if !o.exists {
        Some(Gap::Missing)
    } else if !o.is_hayate {
        Some(Gap::NotHayate)
    } else if o.unknown {
        Some(Gap::UnknownShape)
    } else if !o.enabled {
        Some(Gap::Disabled)
    } else if !o.has_resident() {
        Some(Gap::NoResident)
    } else if !o.has_flag() {
        Some(Gap::NoFlag)
    } else {
        None
    }
}

/// What the "Run at sign-in" row of the Startup tab says and offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signin {
    /// nothing to add: the switch is off and nobody asked, or the shortcut is not known yet
    Quiet,
    /// ON and the shortcut is right
    Ready,
    /// cannot run at sign-in. `why` finishes the status line; `offer` is the inline offer
    Blocked { why: &'static str, offer: Option<Offer> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub text: &'static str,
    /// the one button (None: nothing Hayate may do from here; the text then says what to do)
    pub button: Option<&'static str>,
    /// what the button will do, for its tooltip
    pub tip: &'static str,
}

pub const OFFER_TEXT: &str = "Run at sign-in needs Hayate to start with Windows (hidden in the tray).";
pub const OFFER_BUTTON: &str = "Start Hayate with Windows";
/// every gap on a copy that is not the per-user installed one: text only, no button
const OFFER_NOT_INSTALLED: &str = "Run at sign-in needs Hayate to start with Windows (hidden in the tray). Only the installed Hayate sets that up: open it, or install Hayate from Settings first.";

pub const READY_TEXT: &str = "Runs once at the next sign-in, hidden in the tray. Nothing starts now.";

/// Pure: what to show. `manage` = the switch is ON; `asked` = the user pressed ON and it did not stick
/// (so the offer can show while the switch is still OFF); `installed` = this is the per-user installed
/// copy. Only that copy writes or changes the Startup shortcut: a portable copy would point it at
/// wherever it happens to sit, or switch on / flag a shortcut that points at the old installed exe.
/// Every gap is text only (no button) on any other copy.
/// `known` = the scan has arrived (before that nothing is shown, never a false "missing").
pub fn signin(manage: bool, asked: bool, known: bool, own: Option<&OwnAutorun>, elevated: Option<bool>, installed: bool) -> Signin {
    if !known || !(manage || asked) {
        return Signin::Quiet;
    }
    let unfixable = |why: &'static str| Signin::Blocked { why, offer: None };
    let fix = |tip: &'static str| Some(if installed { Offer { text: OFFER_TEXT, button: Some(OFFER_BUTTON), tip } } else { Offer { text: OFFER_NOT_INSTALLED, button: None, tip: "" } });
    match gap(own, elevated) {
        None if manage => Signin::Ready,
        None => Signin::Quiet,
        Some(Gap::Unchecked) => unfixable("Hayate is running as administrator, so it does not read or change its Start with Windows shortcut. Use a normal Hayate."),
        Some(Gap::Unreadable) => unfixable("Hayate could not read its Start with Windows shortcut."),
        Some(Gap::NotHayate) => unfixable("Hayate.lnk in your Startup folder does not start Hayate, so Hayate will not change it."),
        Some(Gap::NoResident) => unfixable("Hayate.lnk in your Startup folder does not start Hayate hidden (--resident), so Hayate will not change it."),
        Some(Gap::Missing) => Signin::Blocked {
            why: "Hayate does not start with Windows (no Hayate.lnk in your Startup folder).",
            offer: fix("Adds Hayate's Startup shortcut: hidden in the tray, never as administrator. Same shortcut Settings > Install writes."),
        },
        Some(Gap::UnknownShape) => unfixable("Windows holds a setting for Start with Windows in a form Hayate does not recognise: change it in Task Manager > Startup apps."),
        Some(Gap::Disabled) => Signin::Blocked { why: "Start with Windows is switched off in Windows (Task Manager > Startup apps).", offer: fix("Switches Hayate's Startup entry back on in Windows, like Task Manager does, and nothing else.") },
        Some(Gap::NoFlag) => Signin::Blocked { why: "Hayate's Start with Windows shortcut is not set up for the list (a reinstall resets it).", offer: fix("Marks Hayate's existing Startup shortcut for the list. Its other arguments stay.") },
    }
}

/// Read the shortcut. None when it cannot be read at all.
pub fn read(loc: &Locations) -> Option<OwnAutorun> {
    let p = path(loc);
    if !p.is_file() {
        return Some(OwnAutorun::missing());
    }
    let live = approved::read(loc, &id()).ok()?;
    let t = scan::resolve_link(&p.to_string_lossy())?;
    let st = live.effective();
    Some(OwnAutorun { exists: true, enabled: st == State::Enabled, unknown: st == State::Unknown, is_hayate: scan::is_hayate_image(&t.path), args: t.args })
}

/// New argument text for the shortcut, keeping every unrelated argument.
pub fn with_flag(args: &str, on: bool) -> String {
    let mut parts: Vec<&str> = args.split_whitespace().filter(|a| *a != LOGON_FLAG).collect();
    if on {
        parts.push(LOGON_FLAG);
    }
    parts.join(" ")
}

fn sta<R>(f: impl FnOnce() -> R) -> R {
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok();
        let r = f();
        if init {
            CoUninitialize();
        }
        r
    }
}

/// Put the flag on (or take it off) the shortcut. Refuses (changes nothing) when
/// the shortcut is missing, does not start Hayate, or is disabled in Windows (a
/// startup manager that could never run would only look on). `Err` = the file is
/// exactly as it was.
pub fn set_flag(loc: &Locations, on: bool) -> Result<(), String> {
    let p = path(loc);
    let cur = read(loc).ok_or("Hayate could not read its Start with Windows shortcut.")?;
    if !cur.exists {
        return if on { Err("Start with Windows is off, and the startup manager runs through that shortcut.".into()) } else { Ok(()) };
    }
    if !cur.is_hayate {
        return Err("The Start with Windows shortcut does not start Hayate, so Hayate will not change it.".into());
    }
    if on && cur.unknown {
        return Err("Windows holds a setting for Start with Windows in a form Hayate does not recognise; it will not overwrite it. Change it in Task Manager.".into());
    }
    if on && !cur.enabled {
        return Err("Start with Windows is switched off in Windows (Task Manager > Startup apps). Switch it on there first.".into());
    }
    if on == cur.has_flag() {
        return Ok(());
    }
    let new_args = with_flag(&cur.args, on);
    let before = std::fs::read(&p).map_err(|e| format!("cannot read the shortcut file: {e}"))?;
    let saved = sta(|| unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.message())?;
        let pf: IPersistFile = sl.cast().map_err(|e| e.message())?;
        let w = wide(&p.to_string_lossy());
        pf.Load(pcw(&w), STGM_READ).map_err(|e| e.message())?;
        sl.SetArguments(&HSTRING::from(new_args.as_str())).map_err(|e| e.message())?;
        pf.Save(&HSTRING::from(p.as_os_str()), true).map_err(|e| e.message())
    });
    let verify = || read(loc).is_some_and(|a| a.has_flag() == on && a.is_hayate && a.has_resident() == cur.has_resident());
    match saved {
        Ok(()) if verify() => Ok(()),
        other => {
            // saving failed or did not read back: put the exact old bytes back
            let why = match other {
                Err(e) => e,
                Ok(()) => "the saved shortcut did not read back as expected".to_string(),
            };
            let restored = crate::osutil::write_atomic(&p, &before).is_ok();
            Err(format!("Could not change the Start with Windows shortcut ({why}). {}", if restored { "It was left as it was." } else { "It could not be put back; check Hayate.lnk in your Startup folder." }))
        }
    }
}

/// Windows' own on/off for Hayate.lnk: back on, the way Task Manager does it.
/// Refuses a value Hayate does not recognise (never overwritten).
fn enable(loc: &Locations) -> Result<(), String> {
    let live = approved::read(loc, &id()).map_err(|e| format!("cannot read the startup setting: {}", e.text()))?;
    match live.effective() {
        State::Enabled => return Ok(()),
        State::Unknown => return Err("Windows holds a setting for Start with Windows in a form Hayate does not recognise; it will not overwrite it. Change it in Task Manager.".into()),
        State::Disabled => {}
    }
    approved::write(loc, &id(), Some(&approved::enabled_bytes())).map_err(|e| e.text())?;
    match approved::read(loc, &id()) {
        Ok(l) if l.effective() == State::Enabled => Ok(()),
        _ => Err("Windows did not keep the change.".into()),
    }
}

/// The Startup tab's offer, one explicit click: bring Hayate's own Start with
/// Windows shortcut to what the logon run needs. Reads the real state again
/// (the tab's copy may be old), then does only what is missing, in this order:
///
/// 1. no shortcut: `create` writes it (Settings' own write, `--resident`, never elevated);
/// 2. disabled in Windows: switch it back on (StartupApproved, like Task Manager);
/// 3. `flag`: the private argument (only when the list is ON; the flag never appears while it is off).
///
/// A shortcut that does not start Hayate, or does not start it hidden, is never
/// rewritten. No elevation, task or second autorun is involved. The gates are
/// checked here, first, before anything is read or written (not only by the
/// caller): this must be the per-user installed copy and provably not elevated.
/// `Err` leaves whatever the earlier steps wrote as it is (each step is verified
/// on its own).
pub fn make_ready(loc: &Locations, flag: bool, create: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    make_ready_as(loc, flag, Gate::current(), create)
}

/// Who is asking. `current()` is the real process; tests inject the other cases.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// the per-user installed copy (not portable, not a per-machine copy)
    pub installed: bool,
    pub elevated: Option<bool>,
}

impl Gate {
    pub fn current() -> Gate {
        Gate { installed: crate::setup::per_user_installed(), elevated: crate::osutil::elevation() }
    }
}

fn make_ready_as(loc: &Locations, flag: bool, gate: Gate, create: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    if !gate.installed {
        return Err("This copy of Hayate is not the installed one, so it will not write or change its Start with Windows shortcut. Open the installed Hayate, or install Hayate from Settings first.".into());
    }
    if gate.elevated != Some(false) {
        return Err("Hayate is running as administrator (or could not tell), so it will not write or change its Start with Windows shortcut. Do it from a normal Hayate.".into());
    }
    let unreadable = || "Hayate could not read its Start with Windows shortcut.".to_string();
    let mut cur = read(loc).ok_or_else(unreadable)?;
    if !cur.exists {
        create()?;
        cur = read(loc).ok_or_else(unreadable)?;
        if !cur.exists {
            return Err("Hayate wrote its Start with Windows shortcut but could not find it again.".into());
        }
    }
    if !cur.is_hayate {
        return Err("Hayate.lnk in your Startup folder does not start Hayate, so Hayate will not change it.".into());
    }
    if !cur.has_resident() {
        return Err("Hayate.lnk in your Startup folder does not start Hayate hidden (--resident), so Hayate will not change it. Delete it and use the offer again to get a new one.".into());
    }
    if !cur.enabled {
        enable(loc)?;
    }
    if flag {
        set_flag(loc, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::reg::scratch::Scratch;

    /// the only caller make_ready serves: the per-user installed copy, provably not elevated
    const OK: Gate = Gate { installed: true, elevated: Some(false) };

    fn mk(tag: &str) -> (Scratch, PathBuf, Locations) {
        let s = Scratch::new(tag);
        let dir = std::env::temp_dir().join(format!("HayateStartupFixture-own-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("user")).unwrap();
        std::fs::create_dir_all(dir.join("common")).unwrap();
        let loc = Locations { hkcu: s.user.0, hklm: s.machine.0, user_dir: dir.join("user"), common_dir: dir.join("common") };
        (s, dir, loc)
    }

    /// A real .lnk at `loc`'s Startup folder starting this test exe renamed as hayate.exe is not possible;
    /// the shortcut points at a file called hayate.exe in the fixture dir (is_hayate_image goes by name).
    fn write_lnk(loc: &Locations, dir: &PathBuf, args: &str) {
        let exe = dir.join("hayate.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let p = path(loc);
        sta(|| unsafe {
            let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            sl.SetPath(&HSTRING::from(exe.as_os_str())).unwrap();
            sl.SetArguments(&HSTRING::from(args)).unwrap();
            let pf: IPersistFile = sl.cast().unwrap();
            pf.Save(&HSTRING::from(p.as_os_str()), true).unwrap();
        });
    }

    #[test]
    fn flag_text_keeps_unrelated_arguments() {
        assert_eq!(with_flag("--resident", true), "--resident --startup-logon");
        assert_eq!(with_flag("--resident --tab cpu", true), "--resident --tab cpu --startup-logon");
        assert_eq!(with_flag("--resident --startup-logon --tab cpu", false), "--resident --tab cpu");
        assert_eq!(with_flag("--resident --startup-logon", true), "--resident --startup-logon", "never twice");
    }

    #[test]
    fn turning_the_manager_on_adds_the_flag_and_off_removes_only_it() {
        let (_s, dir, loc) = mk("flag");
        write_lnk(&loc, &dir, "--resident --keep-me");
        let before = read(&loc).unwrap();
        assert!(before.exists && before.enabled && before.is_hayate && !before.has_flag() && !before.ok());
        set_flag(&loc, true).unwrap();
        let on = read(&loc).unwrap();
        assert!(on.ok(), "{on:?}");
        assert!(on.args.contains("--keep-me"), "an unrelated argument is kept: {on:?}");
        set_flag(&loc, false).unwrap();
        let off = read(&loc).unwrap();
        assert!(!off.has_flag() && off.has_resident() && off.args.contains("--keep-me"), "{off:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_disabled_or_foreign_shortcut_is_refused_and_untouched() {
        let (_s, dir, loc) = mk("refuse");
        assert!(set_flag(&loc, true).unwrap_err().contains("Start with Windows"), "no shortcut");
        assert!(set_flag(&loc, false).is_ok(), "nothing to take the flag out of");
        write_lnk(&loc, &dir, "--resident");
        // disabled in Windows: StartupApproved says so
        approved::write(&loc, &id(), Some(&approved::disabled_bytes(approved::filetime_now()))).unwrap();
        let bytes = std::fs::read(path(&loc)).unwrap();
        assert!(set_flag(&loc, true).unwrap_err().contains("switched off in Windows"));
        assert_eq!(std::fs::read(path(&loc)).unwrap(), bytes, "not touched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn own(exists: bool, enabled: bool, is_hayate: bool, args: &str) -> OwnAutorun {
        OwnAutorun { exists, enabled, unknown: false, is_hayate, args: args.into() }
    }

    /// StartupApproved holds a shape Hayate does not know: not enabled, but not "switched off" either
    fn unknown_shape(args: &str) -> OwnAutorun {
        OwnAutorun { exists: true, enabled: false, unknown: true, is_hayate: true, args: args.into() }
    }

    fn blocked(s: &Signin) -> (&'static str, Option<&Offer>) {
        match s {
            Signin::Blocked { why, offer } => (*why, offer.as_ref()),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    /// The whole table: switch x shortcut state -> what the row says and offers.
    #[test]
    fn signin_hint_matrix() {
        let good = own(true, true, true, "--resident --startup-logon");
        let (el, inst) = (Some(false), true);
        // right shortcut: ON says ready (wording), OFF says nothing
        assert_eq!(signin(true, false, true, Some(&good), el, inst), Signin::Ready);
        assert!(READY_TEXT.contains("once at the next sign-in") && READY_TEXT.contains("hidden"));
        assert_eq!(signin(false, false, true, Some(&good), el, inst), Signin::Quiet);
        // switch OFF and nobody asked: never a hint, whatever the shortcut looks like
        for o in [own(false, false, false, ""), own(true, false, true, "--resident"), own(true, true, true, "--resident")] {
            assert_eq!(signin(false, false, true, Some(&o), el, inst), Signin::Quiet, "{o:?}");
        }
        // the scan has not arrived: no false "missing"
        assert_eq!(signin(true, true, false, None, el, inst), Signin::Quiet);
        // missing: offer with the button when installed, text only when portable
        let m = own(false, false, false, "");
        let s = signin(true, false, true, Some(&m), el, inst);
        let (why, offer) = blocked(&s);
        assert!(why.contains("does not start with Windows"), "{why}");
        let o = offer.unwrap();
        assert_eq!((o.text, o.button), (OFFER_TEXT, Some(OFFER_BUTTON)));
        assert_eq!(OFFER_TEXT, "Run at sign-in needs Hayate to start with Windows (hidden in the tray).");
        assert_eq!(OFFER_BUTTON, "Start Hayate with Windows");
        let s = signin(true, false, true, Some(&m), el, false);
        let o = blocked(&s).1.unwrap();
        assert!(o.button.is_none() && o.text.contains("install Hayate from Settings"), "a portable copy never writes a Startup shortcut: {o:?}");
        // disabled in Windows
        let s = signin(true, false, true, Some(&own(true, false, true, "--resident --startup-logon")), el, inst);
        let (why, offer) = blocked(&s);
        assert!(why.contains("switched off in Windows"), "{why}");
        assert_eq!(offer.unwrap().button, Some(OFFER_BUTTON));
        // shortcut without the private flag (a reinstall resets it)
        let s = signin(true, false, true, Some(&own(true, true, true, "--resident")), el, inst);
        let (why, offer) = blocked(&s);
        assert!(why.contains("not set up for the list"), "{why}");
        assert_eq!(offer.unwrap().button, Some(OFFER_BUTTON));
        // the user pressed ON, it did not stick (switch still OFF): the offer shows, the shortcut is missing
        let s = signin(false, true, true, Some(&m), el, inst);
        assert_eq!(blocked(&s).1.unwrap().button, Some(OFFER_BUTTON));
        // ... and once the shortcut is right again there is nothing to say while the switch is OFF
        assert_eq!(signin(false, true, true, Some(&good), el, inst), Signin::Quiet);
        // shortcuts Hayate must not rewrite: text only, no button
        for o in [own(true, true, false, "--resident --startup-logon"), own(true, true, true, "--startup-logon")] {
            let s = signin(true, false, true, Some(&o), el, inst);
            let (why, offer) = blocked(&s);
            assert!(offer.is_none() && why.contains("will not change it"), "{o:?}: {why}");
        }
        // elevated or unknown elevation: never offers, even for a missing shortcut
        for e in [Some(true), None] {
            let s = signin(true, false, true, Some(&m), e, inst);
            let (why, offer) = blocked(&s);
            assert!(offer.is_none() && why.contains("administrator"), "{e:?}: {why}");
        }
        // unreadable shortcut
        let s = signin(true, false, true, None, el, inst);
        let (why, offer) = blocked(&s);
        assert!(offer.is_none() && why.contains("could not read"), "{why}");
    }

    /// A portable (or per-machine) copy gets text for EVERY gap and never a button: a button would
    /// re-enable or flag a shortcut that points at the old installed exe, or write one to where it sits.
    #[test]
    fn a_copy_that_is_not_the_installed_one_never_gets_a_button() {
        let el = Some(false);
        let cases = [
            ("missing", own(false, false, false, "")),
            ("disabled", own(true, false, true, "--resident --startup-logon")),
            ("no flag", own(true, true, true, "--resident")),
        ];
        for (name, o) in &cases {
            // installed: the one button
            let s = signin(true, false, true, Some(o), el, true);
            assert_eq!(blocked(&s).1.unwrap().button, Some(OFFER_BUTTON), "{name}: installed copy");
            // not installed: text, no button, no tooltip action
            for manage in [true, false] {
                let s = signin(manage, !manage, true, Some(o), el, false);
                let offer = blocked(&s).1.unwrap_or_else(|| panic!("{name}: the text still says what to do"));
                assert!(offer.button.is_none() && offer.tip.is_empty(), "{name}: {offer:?}");
                assert!(offer.text.contains("install Hayate from Settings"), "{name}: {}", offer.text);
            }
        }
    }

    /// An unreadable StartupApproved shape is not "switched off in Windows": text only, never a dead button.
    #[test]
    fn an_unknown_startup_approved_shape_is_text_only() {
        let o = unknown_shape("--resident --startup-logon");
        assert_eq!(gap(Some(&o), Some(false)), Some(Gap::UnknownShape));
        for inst in [true, false] {
            let s = signin(true, false, true, Some(&o), Some(false), inst);
            let (why, offer) = blocked(&s);
            assert!(offer.is_none(), "installed={inst}: no button, no offer");
            assert!(why.contains("does not recognise") && why.contains("Task Manager") && !why.contains("switched off"), "{why}");
        }
        // a recognised "disabled" still gets the button
        let d = own(true, false, true, "--resident --startup-logon");
        assert_eq!(gap(Some(&d), Some(false)), Some(Gap::Disabled));
        // from_scan/read tell the two apart
        use crate::startup::approved::Live;
        use crate::startup::scan::{Entry, Target};
        let e = Entry {
            id: id(),
            raw: String::new(),
            command: String::new(),
            target: Some(Target { path: r"C:\b\hayate.exe".into(), args: "--resident".into(), dir: String::new(), icon: String::new() }),
            icon: String::new(),
            live: Live::new(true, Some(vec![9, 9, 9]), None),
            is_hayate: true,
            sig: None,
        };
        let o = from_scan(&[e], &[], &PathBuf::from(r"C:\Users\x\Startup")).unwrap();
        assert!(o.unknown && !o.enabled, "{o:?}");
    }

    #[test]
    fn the_scan_view_of_the_shortcut() {
        use crate::startup::approved::Live;
        use crate::startup::scan::{Entry, Target};
        let dir = PathBuf::from(r"C:\Users\x\Startup");
        let lnk = |state: Option<Vec<u8>>, args: &str, hayate: bool| Entry {
            id: id(),
            raw: String::new(),
            command: String::new(),
            target: Some(Target { path: r"C:\b\hayate.exe".into(), args: args.into(), dir: String::new(), icon: String::new() }),
            icon: String::new(),
            live: Live::new(true, state, None),
            is_hayate: hayate,
            sig: None,
        };
        assert_eq!(from_scan(&[], &[], &dir), Some(own(false, false, false, "")), "nothing in the folder = missing");
        let e = lnk(None, "--resident --startup-logon", true);
        let o = from_scan(&[e], &[], &dir).unwrap();
        assert!(o.ok(), "{o:?}");
        let e = lnk(Some(approved::disabled_bytes(5)), "--resident", true);
        let o = from_scan(&[e], &[], &dir).unwrap();
        assert!(o.exists && !o.enabled && o.has_resident() && !o.has_flag());
        // unknown, never "missing": folder unreadable, or a shortcut whose target was not read
        assert_eq!(from_scan(&[], &[format!(r"{}: access denied", dir.display())], &dir), None);
        assert_eq!(from_scan(&[], &["User Startup folder: path unknown".into()], &dir), None);
        let mut e = lnk(None, "", true);
        e.target = None;
        assert_eq!(from_scan(&[e], &[], &dir), None);
    }

    #[test]
    fn make_ready_creates_flags_and_enables_only_what_is_missing() {
        let (_s, dir, loc) = mk("ready");
        let made = std::cell::Cell::new(0);
        // 1. nothing there: the creator runs once, then the flag (list ON)
        make_ready_as(&loc, true, OK, || {
            made.set(made.get() + 1);
            write_lnk(&loc, &dir, "--resident");
            Ok(())
        })
        .unwrap();
        assert_eq!(made.get(), 1);
        assert!(read(&loc).unwrap().ok());
        // 2. already right: nothing is created or rewritten
        let bytes = std::fs::read(path(&loc)).unwrap();
        make_ready_as(&loc, true, OK, || panic!("must not write a shortcut that exists")).unwrap();
        assert_eq!(std::fs::read(path(&loc)).unwrap(), bytes);
        // 3. disabled in Windows: switched back on, shortcut file untouched when the list is OFF
        approved::write(&loc, &id(), Some(&approved::disabled_bytes(approved::filetime_now()))).unwrap();
        set_flag_for_test_off(&loc);
        make_ready_as(&loc, false, OK, || panic!("exists")).unwrap();
        let o = read(&loc).unwrap();
        assert!(o.enabled && !o.has_flag(), "enabled, and no flag while the list is off: {o:?}");
        // 4. list ON: flag added, unrelated arguments kept
        write_lnk(&loc, &dir, "--resident --keep-me");
        make_ready_as(&loc, true, OK, || panic!("exists")).unwrap();
        let o = read(&loc).unwrap();
        assert!(o.ok() && o.args.contains("--keep-me"), "{o:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gates are inside make_ready: a portable / per-machine copy, an elevated Hayate and one that
    /// cannot tell all get an error with NOTHING read or written (creator not run, registry and file untouched).
    #[test]
    fn make_ready_refuses_unless_installed_and_provably_not_elevated() {
        let (_s, dir, loc) = mk("ready-gate");
        let bad = [
            ("portable", Gate { installed: false, elevated: Some(false) }, "not the installed one"),
            ("elevated", Gate { installed: true, elevated: Some(true) }, "administrator"),
            ("elevation unknown", Gate { installed: true, elevated: None }, "administrator"),
            ("portable and elevated", Gate { installed: false, elevated: Some(true) }, "not the installed one"),
        ];
        // 1. no shortcut: the creator must not run
        for (name, g, why) in &bad {
            let e = make_ready_as(&loc, true, *g, || panic!("{name}: the creator ran")).unwrap_err();
            assert!(e.contains(why), "{name}: {e}");
            assert!(!path(&loc).exists(), "{name}: nothing written");
        }
        // 2. a shortcut that points at the OLD installed exe, disabled and unflagged: not re-enabled, not flagged
        write_lnk(&loc, &dir, "--resident");
        approved::write(&loc, &id(), Some(&approved::disabled_bytes(approved::filetime_now()))).unwrap();
        let bytes = std::fs::read(path(&loc)).unwrap();
        let live = approved::read(&loc, &id()).unwrap().effective();
        for (name, g, _) in &bad {
            for flag in [true, false] {
                assert!(make_ready_as(&loc, flag, *g, || panic!("{name}: the creator ran")).is_err(), "{name}");
            }
        }
        assert_eq!(std::fs::read(path(&loc)).unwrap(), bytes, "shortcut file untouched");
        assert_eq!(approved::read(&loc, &id()).unwrap().effective(), live, "StartupApproved untouched (still disabled)");
        assert!(!read(&loc).unwrap().enabled);
        // the same call with the real gate open works, so the refusals above were the gate and nothing else
        make_ready_as(&loc, true, OK, || panic!("exists")).unwrap();
        assert!(read(&loc).unwrap().ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// make_ready (the public one) reads the real process: the test exe is never the per-user installed copy,
    /// so it must refuse before touching anything.
    #[test]
    fn make_ready_with_the_real_gate_refuses_a_test_binary() {
        let (_s, dir, loc) = mk("ready-real");
        let e = make_ready(&loc, true, || panic!("the creator ran")).unwrap_err();
        assert!(e.contains("not the installed one"), "{e}");
        assert!(!path(&loc).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_flag_for_test_off(loc: &Locations) {
        set_flag(loc, false).unwrap();
    }

    #[test]
    fn make_ready_refuses_what_it_must_not_rewrite_and_reports_failures() {
        let (_s, dir, loc) = mk("ready-refuse");
        // the creator fails: nothing else happens, the error says why
        let e = make_ready_as(&loc, true, OK, || Err("Cannot write the Startup shortcut: denied".into())).unwrap_err();
        assert!(e.contains("denied"), "{e}");
        assert!(!read(&loc).unwrap().exists);
        // a creator that "succeeds" without writing is caught
        assert!(make_ready_as(&loc, true, OK, || Ok(())).unwrap_err().contains("could not find it again"));
        // no --resident: never rewritten
        write_lnk(&loc, &dir, "--keep-me");
        let bytes = std::fs::read(path(&loc)).unwrap();
        assert!(make_ready_as(&loc, true, OK, || panic!("exists")).unwrap_err().contains("--resident"));
        assert_eq!(std::fs::read(path(&loc)).unwrap(), bytes, "not touched");
        // a StartupApproved value of a shape Hayate does not know is never overwritten
        write_lnk(&loc, &dir, "--resident");
        approved::write(&loc, &id(), Some(&approved::disabled_bytes(approved::filetime_now()))).unwrap();
        let k = crate::startup::reg::create(loc.hkcu, &Locations::approved_path(&id()), windows::Win32::System::Registry::KEY_SET_VALUE | windows::Win32::System::Registry::KEY_WOW64_64KEY).unwrap();
        crate::startup::reg::set_binary(&k, LINK_NAME, &[9, 9, 9]).unwrap();
        let e = make_ready_as(&loc, true, OK, || panic!("exists")).unwrap_err();
        assert!(e.contains("does not recognise") || e.contains("cannot read"), "{e}");
        assert!(!read(&loc).map(|o| o.has_flag()).unwrap_or(false), "no flag added after a refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_that_fails_leaves_the_exact_old_file() {
        let (_s, dir, loc) = mk("rollback");
        write_lnk(&loc, &dir, "--resident");
        let p = path(&loc);
        let bytes = std::fs::read(&p).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true); // IPersistFile::Save cannot replace it
        std::fs::set_permissions(&p, perm.clone()).unwrap();
        let err = set_flag(&loc, true).unwrap_err();
        assert!(err.contains("Could not change"), "{err}");
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), bytes, "the shortcut is byte-for-byte as before");
        assert!(!read(&loc).unwrap().has_flag());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
