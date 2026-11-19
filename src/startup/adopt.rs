//! Changing startup entries: enable/disable an ordinary entry, and take an
//! entry over ("manage") or give it back ("unmanage").
//!
//! The only thing Hayate ever changes about an existing entry is its
//! StartupApproved value (the same switch Task Manager flips). It never deletes
//! or rewrites a Run value or a shortcut file.
//!
//! Taking over, in this order:
//!   1. read the entry's live state and its source (Run value type + bytes, or
//!      the shortcut file's bytes) and refuse if either is not what the list
//!      showed (an installer may have replaced the program);
//!   2. refuse early (no prompt, no ACL change) if this process cannot write it;
//!   3. save the ownership record (original bytes + the bytes Hayate will write)
//!      durably (temp write, flush, write-through replace, read back);
//!   4. re-read the source once more, and only then disable the original;
//!   5. read back (state and source again), and mark the record Owned.
//! A failure in 4 or 5 puts the registry back (only while it still holds exactly
//! what Hayate wrote) and removes the half-created managed entry, so a failed
//! take-over never leaves an enabled managed app next to an enabled original. A
//! crash between 3 and 5 leaves a Pending record that [`reconcile`] resolves from
//! what the registry says, without writing the registry.
//!
//! Giving back restores EXACTLY the recorded original, and only while the live
//! value is still exactly what Hayate wrote. Anything else means somebody (the
//! user in Task Manager, an installer) changed it since: it is left alone.
//! [`release_all`] does that for every owned entry; the uninstaller calls it
//! first and removes nothing if it fails.
//!
//! Every operation holds [`Store::ops`], so two of them (for instance one on an
//! I/O worker the watchdog abandoned and one on its replacement) never
//! interleave their registry and record steps.

use super::approved::{self, Live, Locations, State};
use super::id::{EntryId, Scope};
use super::model::{App, Own, OwnState};
use super::scan::{self, Entry, SourceSig};
use super::store::Store;

#[cfg(test)]
thread_local! {
    /// test-only: make the next registry writes fail
    pub static FAIL_REGISTRY_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// test-only: run this closure right after the original was disabled (an
    /// installer replacing the program at the worst moment)
    pub static AFTER_DISABLE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    /// test-only: StartupApproved writes attempted on this thread
    pub static WRITES: std::cell::RefCell<Vec<EntryId>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn put(loc: &Locations, id: &EntryId, v: Option<&[u8]>) -> Result<(), String> {
    #[cfg(test)]
    WRITES.with(|w| w.borrow_mut().push(id.clone()));
    #[cfg(test)]
    if FAIL_REGISTRY_WRITE.with(|f| f.get()) {
        return Err("injected registry failure".into());
    }
    approved::write(loc, id, v).map_err(|e| e.text())
}

const NEEDS_ADMIN: &str = "Changing this entry needs administrator rights. Use Restart as admin, then try again.";
const CHANGED: &str = "This entry changed since the list was read. Refresh and try again.";

/// Can this process change the entry? Err = say why, before anything is saved.
fn can_write(loc: &Locations, id: &EntryId) -> Result<(), String> {
    approved::writable(loc, id).map_err(|e| if e.denied() { NEEDS_ADMIN.to_string() } else { format!("cannot open the startup settings: {}", e.text()) })
}

fn state_word(s: State) -> &'static str {
    match s {
        State::Enabled => "enabled",
        State::Disabled => "disabled",
        State::Unknown => "in a form Hayate does not recognise",
    }
}

fn value_path(id: &EntryId) -> String {
    let root = if id.scope == Scope::User { "HKCU" } else { "HKLM" };
    format!(r"{root}\{}", Locations::approved_path(id))
}

/// The list's `live` and the registry agree on every part of the approval state.
fn same_live(a: &Live, b: &Live) -> bool {
    a.approved == b.approved && a.user_override == b.user_override && a.wrong_type == b.wrong_type
}

/// `e.raw` must be what its recorded source says (a hand-built or stale entry
/// cannot slip another command into the ownership record).
fn raw_matches(e: &Entry, loc: &Locations, sig: &SourceSig) -> bool {
    match sig {
        SourceSig::Run(ty, b) => super::reg::text(&(*ty, b.clone())).is_some_and(|t| t == e.raw),
        SourceSig::File(..) => loc.dir(e.id.scope).join(&e.id.key).to_string_lossy() == e.raw.as_str(),
    }
}

/// What taking an entry over would change, in words. Changes nothing.
pub struct Plan {
    pub lines: Vec<String>,
    /// why it cannot be done (None = ready)
    pub blocked: Option<String>,
}

pub fn preview(loc: &Locations, e: &Entry, store: &Store) -> Plan {
    let mut lines = Vec::new();
    let id = &e.id;
    let blocked = (|| -> Option<String> {
        if e.is_hayate {
            return Some("This entry starts Hayate itself. Use the Start with Windows switch instead.".into());
        }
        if store.snapshot().cfg.owner_of(id).is_some() {
            return Some("Hayate already manages this entry.".into());
        }
        let live = match approved::read(loc, id) {
            Ok(l) => l,
            Err(e) => return Some(format!("cannot read this entry: {}", e.text())),
        };
        if !live.present {
            return Some("This entry no longer exists.".into());
        }
        if id.scope == Scope::AllUsers && live.user_override.is_some() {
            return Some("This all-users entry also has this user's own startup setting, which Windows applies on top. Hayate will not decide between them; change it in Task Manager.".into());
        }
        if live.effective() == State::Unknown {
            return Some("Windows holds a startup setting for it in a form Hayate does not recognise; it will not overwrite that.".into());
        }
        if e.sig.is_none() {
            return Some("Hayate could not read this entry's command, so it will not take it over.".into());
        }
        can_write(loc, id).err()
    })();
    lines.push(format!("1. Hayate saves a copy of this entry's command into your Hayate startup list ({}).", id.source_label()));
    lines.push(format!("2. Only after that is saved to disk, the original is switched off: {} \\ {} = disabled (now: {}).", value_path(id), id.key, state_word(approved::state_of(e.live.approved.as_deref()))));
    lines.push("3. The original Run value or shortcut file is not deleted or edited. If it changes while Hayate works, Hayate stops and puts the switch back.".into());
    lines.push("4. Unmanage (and Uninstall) put back exactly that one value, and only if nothing else has changed it since.".into());
    if id.scope == Scope::AllUsers {
        lines.push(format!(r"   For this all-users entry Hayate also keeps a copy of that record in HKLM\{}, which only administrators can change; it is given back only while that copy matches.", super::proof::KEY));
    }
    lines.push("5. It starts only when you turn on the startup manager and Hayate runs at logon, or when you press Run now.".into());
    Plan { lines, blocked }
}

#[derive(Debug, PartialEq)]
pub struct Adopted {
    pub app_id: String,
    pub note: String,
}

/// Undo the half-made managed entry of a failed take-over: remove it from the
/// list (it was created by this very take-over). `Ok` only once saved.
fn drop_app(store: &Store, app_id: &str) -> Result<(), String> {
    let rid = app_id.to_string();
    store.update_sync(|c| {
        c.apps.retain(|a| a.id != rid);
        for a in c.apps.iter_mut() {
            if a.after.as_deref() == Some(rid.as_str()) {
                a.after = None;
            }
        }
    })
}

/// Take `e` over as the managed entry `app` (id is assigned here).
pub fn adopt(loc: &Locations, store: &Store, e: &Entry, mut app: App) -> Result<Adopted, String> {
    let _ops = store.ops();
    let plan = preview(loc, e, store);
    if let Some(b) = plan.blocked {
        return Err(b);
    }
    let id = e.id.clone();
    let sig = e.sig.clone().ok_or("Hayate could not read this entry's command, so it will not take it over.")?;
    // the list the user looked at must still be the live state AND the same program
    let live = approved::read(loc, &id).map_err(|e| e.text())?;
    if !live.present || !same_live(&live, &e.live) {
        return Err(CHANGED.into());
    }
    if scan::source_sig(loc, &id).as_ref() != Some(&sig) || !raw_matches(e, loc, &sig) {
        return Err(CHANGED.into());
    }
    let written = approved::disabled_bytes(approved::filetime_now());
    let own = Own { source: id.clone(), state: OwnState::Pending, original: live.approved.clone(), written: written.clone(), raw_at_adopt: e.raw.clone() };
    let mut app_id = String::new();
    // step 3: the record is durable (flushed, replaced, read back) before the original is touched
    store
        .update_sync(|c| {
            app.id = c.fresh_id();
            app.own = Some(own.clone());
            app_id = app.id.clone();
            c.apps.push(app.clone());
        })
        .map_err(|e| format!("Not changed. The startup list could not be saved: {e}"))?;
    // step 3b, all-users only: the administrator-protected copy, without which the
    // record would never be trusted to give the entry back (see `proof`)
    let machine = id.scope == Scope::AllUsers;
    if machine {
        if let Err(e) = super::proof::save(loc, &id, &written, own.original.as_deref()) {
            super::proof::forget(loc, &id);
            let tail = match drop_app(store, &app_id) {
                Ok(()) => "The original was not changed.",
                Err(_) => "The record stays pending and is cleaned up the next time Hayate starts.",
            };
            return Err(format!(r"Not changed. Hayate could not save its administrator-protected copy of the record in HKLM\{} ({e}). {tail}", super::proof::KEY));
        }
    }
    let forget = || {
        if machine {
            super::proof::forget(loc, &id);
        }
    };
    // step 4: the program must still be the one the user saw
    if scan::source_sig(loc, &id).as_ref() != Some(&sig) {
        forget();
        let tail = match drop_app(store, &app_id) {
            Ok(()) => "The original was not changed.".to_string(),
            Err(_) => "The record stays pending and is cleaned up the next time Hayate starts.".to_string(),
        };
        return Err(format!("The entry was changed by another program while Hayate prepared to take it over. {tail}"));
    }
    if let Err(e) = put(loc, &id, Some(&written)) {
        // roll the record AND the half-made managed entry back: nothing was disabled
        forget();
        let tail = match drop_app(store, &app_id) {
            Ok(()) => "The original was not changed.",
            Err(_) => "The record stays pending and is cleaned up the next time Hayate starts.",
        };
        return Err(format!("Could not disable the original ({e}). {tail}"));
    }
    #[cfg(test)]
    if let Some(f) = AFTER_DISABLE.with(|c| c.borrow_mut().take()) {
        f();
    }
    // step 5
    let confirmed = approved::read(loc, &id).map(|l| !l.wrong_type && l.approved.as_deref() == Some(written.as_slice())).unwrap_or(false);
    if !confirmed {
        return Err("The original did not read back as disabled. It is recorded as pending; check it in Task Manager. Hayate will not start the new entry until that is settled.".into());
    }
    if scan::source_sig(loc, &id).as_ref() != Some(&sig) {
        // replaced while it was being switched off: put the switch back (only if it still holds our bytes) and forget the take-over
        let back = put(loc, &id, live.approved.as_deref());
        let gone = back.is_ok() && drop_app(store, &app_id).is_ok();
        if gone {
            forget(); // otherwise the record stays and still needs its protected copy
        }
        return Err(if gone {
            "The entry was replaced by another program while Hayate took it over. Hayate put it back exactly as it was and did not keep the new entry.".into()
        } else {
            "The entry was replaced by another program while Hayate took it over, and Hayate could not fully put it back. It is recorded as pending; check it in Task Manager.".into()
        });
    }
    let rid = app_id.clone();
    match store.update_sync(|c| {
        if let Some(o) = c.apps.iter_mut().find(|a| a.id == rid).and_then(|a| a.own.as_mut()) {
            o.state = OwnState::Owned;
        }
    }) {
        Ok(()) => Ok(Adopted { app_id, note: "The original is disabled and the entry is managed.".into() }),
        Err(e) => Ok(Adopted { app_id, note: format!("The original is disabled. Confirming it in the list failed ({e}); it is completed at the next start.") }),
    }
}

fn reconcile_inner(loc: &Locations, store: &Store) -> Vec<String> {
    let snap = store.snapshot();
    let mut notes = Vec::new();
    // None = the take-over never happened: drop the record and the managed entry made for it
    let mut decisions: Vec<(String, Option<OwnState>)> = Vec::new();
    // dropped all-users records whose protected copy may go with them (see `proof::forget_proven`)
    let mut dropped_machine: Vec<Own> = Vec::new();
    for a in &snap.cfg.apps {
        let Some(o) = &a.own else { continue };
        if o.state != OwnState::Pending {
            continue;
        }
        let Ok(l) = approved::read(loc, &o.source) else { continue };
        if l.wrong_type {
            notes.push(format!("{}: the original's startup setting is in a form Hayate does not recognise; left as it is", a.name));
        } else if l.approved.as_deref() == Some(o.written.as_slice()) {
            decisions.push((a.id.clone(), Some(OwnState::Owned)));
            notes.push(format!("{}: completed an interrupted take-over", a.name));
        } else if l.approved == o.original {
            decisions.push((a.id.clone(), None));
            if o.source.scope == Scope::AllUsers {
                dropped_machine.push(o.clone());
            }
            notes.push(format!("{}: the original was never disabled; the unfinished take-over was dropped", a.name));
        } else {
            notes.push(format!("{}: the original was changed by something else during a take-over; left as it is", a.name));
        }
    }
    if !decisions.is_empty() {
        let saved = store.update_sync(|c| {
            for (id, d) in &decisions {
                match d {
                    Some(s) => {
                        if let Some(o) = c.apps.iter_mut().find(|a| &a.id == id).and_then(|a| a.own.as_mut()) {
                            o.state = *s;
                        }
                    }
                    None => {
                        c.apps.retain(|a| &a.id != id);
                        for a in c.apps.iter_mut() {
                            if a.after.as_deref() == Some(id.as_str()) {
                                a.after = None;
                            }
                        }
                    }
                }
            }
        });
        if saved.is_ok() {
            // the live value is the original, never Hayate's bytes: the copy can give nothing back
            for o in &dropped_machine {
                super::proof::forget_proven(loc, &o.source, &o.written, o.original.as_deref());
            }
        }
    }
    notes
}

/// Resolve records left Pending by an interrupted take-over, from what the
/// registry says now. Never writes the registry. Returns what it did.
pub fn reconcile(loc: &Locations, store: &Store) -> Vec<String> {
    let _ops = store.ops();
    reconcile_inner(loc, store)
}

#[derive(Debug, PartialEq)]
pub enum Released {
    /// the exact original was put back
    Restored,
    /// the live value was no longer what Hayate wrote: nothing was touched
    ChangedSince(String),
}

/// What a give-back did, inside this module. `Untrusted` is shown to callers
/// outside as [`Released::ChangedSince`] (nothing was touched either way).
#[derive(Debug, PartialEq)]
enum Outcome {
    Done(Released),
    /// an all-users record without a matching administrator-protected copy
    /// (`proof`): it may have been forged by an unelevated process, so nothing
    /// was written and the record was dropped
    Untrusted(String),
}

fn untrusted_why(why: &str) -> String {
    format!("Hayate could not confirm that it made this all-users take-over ({why}), so it did not trust its copy of the record and did not write anything. If the entry should start, turn it on in Task Manager > Startup apps (as administrator)")
}

fn release_inner(loc: &Locations, store: &Store, app_id: &str) -> Result<Released, String> {
    Ok(match release_outcome(loc, store, app_id)? {
        Outcome::Done(r) => r,
        Outcome::Untrusted(why) => Released::ChangedSince(why),
    })
}

fn release_outcome(loc: &Locations, store: &Store, app_id: &str) -> Result<Outcome, String> {
    let snap = store.snapshot();
    let a = snap.cfg.app(app_id).ok_or("no such entry")?;
    let Some(o) = a.own.clone() else { return Ok(Outcome::Done(Released::ChangedSince("it does not own a startup entry".into()))) };
    let live = approved::read(loc, &o.source).map_err(|e| format!("cannot read the startup setting: {}", e.text()))?;
    let machine = o.source.scope == Scope::AllUsers;
    let ours = !live.wrong_type && live.approved.as_deref() == Some(o.written.as_slice());
    if ours && machine {
        // the record comes from a store any unelevated process of this user can
        // write: an all-users (HKLM) value is only ever written on the strength
        // of the administrator-protected copy. No copy = never written, never an
        // error (an uninstall carries on), and the untrusted record is dropped.
        if let Err(why) = super::proof::check(loc, &o.source, &o.written, o.original.as_deref()) {
            let why = untrusted_why(&why);
            crate::osutil::log(&format!("startup: {}: not given back: {why}", a.name));
            let rid = app_id.to_string();
            let _ = store.update_sync(|c| {
                if let Some(a) = c.apps.iter_mut().find(|a| a.id == rid) {
                    a.own = None;
                }
            });
            return Ok(Outcome::Untrusted(why));
        }
    }
    let outcome = if ours {
        can_write(loc, &o.source)?;
        put(loc, &o.source, o.original.as_deref()).map_err(|e| format!("Could not put the original back ({e}). Nothing else was changed."))?;
        let after = approved::read(loc, &o.source).map_err(|e| e.text())?;
        if after.approved != o.original || after.wrong_type {
            return Err("The original did not read back as restored. The record is kept.".into());
        }
        Released::Restored
    } else {
        Released::ChangedSince(match (live.wrong_type, approved::state_of(live.approved.as_deref())) {
            (true, _) | (_, State::Unknown) => "its setting was changed outside Hayate".into(),
            (_, State::Enabled) => "it is enabled again (changed outside Hayate)".into(),
            (_, State::Disabled) => "it was disabled again by something else".into(),
        })
    };
    // the record goes only after the registry step succeeded (or proved unnecessary)
    let rid = app_id.to_string();
    let restored = outcome == Released::Restored;
    store
        .update_sync(|c| {
            if let Some(a) = c.apps.iter_mut().find(|a| a.id == rid) {
                a.own = None;
            }
        })
        .map_err(|e| if restored { format!("The original was put back, but the startup list could not be saved ({e}). Unmanage it again to clear the record.") } else { e })?;
    if machine && restored {
        // only a record the copy just proved may remove it (a forged record must not erase the
        // real one's copy); best effort, a stale copy alone proves nothing
        super::proof::forget(loc, &o.source);
    } else if machine {
        // changed outside Hayate: the record is gone and the live value is no longer Hayate's bytes;
        // its copy goes too, but only when it proves exactly this record (elevated, best effort)
        super::proof::forget_proven(loc, &o.source, &o.written, o.original.as_deref());
    }
    Ok(Outcome::Done(outcome))
}

/// Give the entry back. The ownership record is cleared only after the
/// registry step succeeded (or proved unnecessary).
pub fn release(loc: &Locations, store: &Store, app_id: &str) -> Result<Released, String> {
    let _ops = store.ops();
    release_inner(loc, store, app_id)
}

/// Remove a managed entry from the list: give its original back first.
pub fn remove(loc: &Locations, store: &Store, app_id: &str) -> Result<Option<Released>, String> {
    let _ops = store.ops();
    let owns = store.snapshot().cfg.app(app_id).is_some_and(|a| a.own.is_some());
    let rel = if owns { Some(release_inner(loc, store, app_id)?) } else { None };
    let rid = app_id.to_string();
    store.update_sync(|c| {
        c.apps.retain(|a| a.id != rid);
        for a in c.apps.iter_mut() {
            if a.after.as_deref() == Some(rid.as_str()) {
                a.after = None; // whatever waited for it no longer does
            }
        }
    })?;
    Ok(rel)
}

#[derive(Debug, PartialEq, Default)]
pub struct ReleasedAll {
    pub restored: usize,
    /// owned entries that were not Hayate's to put back any more (changed outside Hayate)
    pub left: Vec<String>,
    /// all-users records Hayate could not prove it made (no matching
    /// administrator-protected copy): nothing written, not a failure
    pub skipped: Vec<String>,
}

impl ReleasedAll {
    /// One plain line for the uninstall log: counts, and what to do about skipped entries.
    pub fn summary(&self) -> String {
        let mut s = format!("{} given back, {} left as changed outside Hayate", self.restored, self.left.len());
        let n = self.skipped.len();
        if n > 0 {
            s.push_str(&format!("; {n} all-users entr{} left disabled: re-enable in Task Manager", if n == 1 { "y" } else { "ies" }));
        }
        s
    }
}

/// Give back EVERY entry Hayate holds, for the uninstaller. Unfinished
/// take-overs are reconciled first. All or nothing as far as the caller is
/// concerned: any failure (no administrator rights for an all-users entry, a
/// registry or list write error, a list Hayate cannot read or that lost
/// ownership records) returns `Err` naming it, and the uninstaller then removes
/// NOTHING. An entry somebody else changed since is not touched and is reported
/// in `left`; it is not a failure (there is nothing of Hayate's to restore). An
/// all-users record without its administrator-protected copy (see `proof`) is
/// never acted on, whatever this process's rights: it is reported in `skipped`
/// and logged, and the uninstall goes on.
pub fn release_all(loc: &Locations, store: &Store) -> Result<ReleasedAll, String> {
    let _ops = store.ops();
    let snap = store.snapshot();
    if !snap.loaded {
        return Err("The startup list is still loading. Try again in a moment. Nothing was removed.".into());
    }
    if let Some(l) = &snap.locked {
        return Err(format!("Hayate cannot read its startup list ({l}), so it cannot tell which Windows startup entries it switched off. Nothing was removed."));
    }
    if snap.dropped_owned > 0 {
        return Err(format!(
            "{} entr{} in the startup list could not be read and may hold a Windows startup entry Hayate switched off (the original text is kept in startup.ini.bak). Nothing was removed.",
            snap.dropped_owned,
            if snap.dropped_owned == 1 { "y" } else { "ies" }
        ));
    }
    reconcile_inner(loc, store);
    let ids: Vec<String> = store.snapshot().cfg.apps.iter().filter(|a| a.own.is_some()).map(|a| a.id.clone()).collect();
    let mut out = ReleasedAll::default();
    let mut failed = Vec::new();
    for id in ids {
        let name = store.snapshot().cfg.app(&id).map(|a| a.name.clone()).unwrap_or_default();
        match release_outcome(loc, store, &id) {
            Ok(Outcome::Done(Released::Restored)) => out.restored += 1,
            Ok(Outcome::Done(Released::ChangedSince(why))) => out.left.push(format!("{name}: left as it is because {why}")),
            Ok(Outcome::Untrusted(why)) => out.skipped.push(format!("{name}: skipped: {why}")),
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    if failed.is_empty() {
        // nothing of this user's is held any more: an empty protected key goes too (elevated only)
        if super::proof::tidy(loc) {
            crate::osutil::log(&format!(r"startup: removed the empty HKLM\{}", super::proof::KEY));
        }
        Ok(out)
    } else {
        Err(format!("Hayate could not give every Windows startup entry back: {}. Nothing was removed.", failed.join("; ")))
    }
}

/// Enable or disable an ordinary (unmanaged) entry the way Task Manager does.
/// `seen` is what the list showed: if the live value is different now, nothing
/// is written (somebody else changed it).
pub fn set_enabled(loc: &Locations, e: &Entry, on: bool, store: &Store) -> Result<(), String> {
    let _ops = store.ops();
    let id = &e.id;
    if store.snapshot().cfg.owner_of(id).is_some() {
        return Err("Hayate manages this entry; unmanage it first.".into());
    }
    let live: Live = approved::read(loc, id).map_err(|e| format!("cannot read the startup setting: {}", e.text()))?;
    if !live.present {
        return Err("This entry no longer exists.".into());
    }
    if !same_live(&live, &e.live) {
        return Err(CHANGED.into());
    }
    if id.scope == Scope::AllUsers && live.user_override.is_some() {
        return Err("This all-users entry also has this user's own startup setting, which Windows applies on top. Change it in Task Manager.".into());
    }
    if live.effective() == State::Unknown {
        return Err("Windows holds a setting for this entry in a form Hayate does not recognise; it will not overwrite it.".into());
    }
    if live.effective() == if on { State::Enabled } else { State::Disabled } {
        return Ok(());
    }
    can_write(loc, id)?;
    let v = if on { approved::enabled_bytes() } else { approved::disabled_bytes(approved::filetime_now()) };
    put(loc, id, Some(&v))?;
    let after = approved::read(loc, id).map_err(|e| e.text())?;
    if after.effective() != if on { State::Enabled } else { State::Disabled } {
        return Err("Windows did not keep the change.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::id::{Kind, View};
    use crate::startup::reg::{self, scratch::Scratch};
    use crate::startup::scan;
    use std::path::PathBuf;
    use std::sync::Arc;
    use windows::Win32::System::Registry::*;

    struct Rig {
        _s: Scratch,
        dir: PathBuf,
        loc: Locations,
        store: Arc<Store>,
    }

    impl Rig {
        fn new(tag: &str) -> Rig {
            let s = Scratch::new(tag);
            let dir = std::env::temp_dir().join(format!("HayateStartupFixture-adopt-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("user")).unwrap();
            std::fs::create_dir_all(dir.join("common")).unwrap();
            let loc = Locations { hkcu: s.user.0, hklm: s.machine.0, user_dir: dir.join("user"), common_dir: dir.join("common") };
            let store = Arc::new(Store::new(dir.join("startup.ini")));
            store.load();
            FAIL_REGISTRY_WRITE.with(|f| f.set(false));
            Rig { _s: s, dir, loc, store }
        }

        fn run_value(&self, scope: Scope, name: &str, cmd: &str) {
            let root = self.loc.root(scope);
            let k = reg::create(root, approved::RUN, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
            let v: Vec<u8> = crate::osutil::wide(cmd).iter().flat_map(|c| c.to_le_bytes()).collect();
            let n = crate::osutil::wide(name);
            unsafe {
                assert_eq!(RegSetValueExW(k.0, crate::osutil::pcw(&n), None, REG_SZ, Some(&v)), windows::Win32::Foundation::ERROR_SUCCESS);
            }
        }

        fn entry(&self, scope: Scope, name: &str) -> Entry {
            let s = scan::scan(&self.loc);
            s.entries.into_iter().find(|e| e.id == EntryId::new(Kind::Run, scope, View::Native, name)).expect("entry")
        }

        fn live(&self, scope: Scope, name: &str) -> Live {
            approved::read(&self.loc, &EntryId::new(Kind::Run, scope, View::Native, name)).unwrap()
        }

        fn on_disk(&self) -> String {
            std::fs::read_to_string(self.dir.join("startup.ini")).unwrap_or_default()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn app(name: &str) -> App {
        App::new(String::new(), name.into(), r"C:\nope\fixture.exe".into())
    }

    const N: &str = "HayateStartupFixture One";

    #[test]
    fn adopt_saves_the_record_first_then_disables_and_unmanage_restores_exactly() {
        let r = Rig::new("adopt-ok");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe --x");
        let e = r.entry(Scope::User, N);
        assert_eq!(e.state(), State::Enabled);
        let plan = preview(&r.loc, &e, &r.store);
        assert!(plan.blocked.is_none(), "{:?}", plan.blocked);
        assert!(plan.lines.iter().any(|l| l.contains("not deleted or edited")));
        assert_eq!(r.live(Scope::User, N).approved, None, "preview wrote nothing");

        let out = adopt(&r.loc, &r.store, &e, app("One")).unwrap();
        let live = r.live(Scope::User, N);
        assert_eq!(approved::state_of(live.approved.as_deref()), State::Disabled);
        let snap = r.store.snapshot();
        let own = snap.cfg.app(&out.app_id).unwrap().own.clone().unwrap();
        assert_eq!(own.state, OwnState::Owned);
        assert_eq!(own.original, None, "there was no value: that is what comes back");
        assert!(r.on_disk().contains("own.state=owned"));
        assert!(r.live(Scope::User, N).present, "the Run value itself is untouched");

        assert_eq!(release(&r.loc, &r.store, &out.app_id).unwrap(), Released::Restored);
        assert_eq!(r.live(Scope::User, N).approved, None, "value removed again: byte-for-byte the starting state");
        assert!(r.store.snapshot().cfg.app(&out.app_id).unwrap().own.is_none());
    }

    #[test]
    fn unmanage_puts_back_a_non_default_original_exactly() {
        let r = Rig::new("adopt-orig");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let original = vec![6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        approved::write(&r.loc, &EntryId::new(Kind::Run, Scope::User, View::Native, N), Some(&original)).unwrap();
        let e = r.entry(Scope::User, N);
        let out = adopt(&r.loc, &r.store, &e, app("One")).unwrap();
        assert_eq!(release(&r.loc, &r.store, &out.app_id).unwrap(), Released::Restored);
        assert_eq!(r.live(Scope::User, N).approved, Some(original));
    }

    #[test]
    fn unmanage_never_clobbers_an_external_change() {
        let r = Rig::new("adopt-ext");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        let out = adopt(&r.loc, &r.store, &e, app("One")).unwrap();
        // the user flips it back on in Task Manager: different bytes than Hayate wrote
        let theirs = vec![6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        approved::write(&r.loc, &e.id, Some(&theirs)).unwrap();
        let rel = release(&r.loc, &r.store, &out.app_id).unwrap();
        assert!(matches!(rel, Released::ChangedSince(ref w) if w.contains("enabled again")), "{rel:?}");
        assert_eq!(r.live(Scope::User, N).approved, Some(theirs), "left exactly as the user set it");
        assert!(r.store.snapshot().cfg.app(&out.app_id).unwrap().own.is_none(), "the record is dropped: nothing left to restore");
    }

    #[test]
    fn adopt_refuses_when_an_installer_replaced_the_source_after_the_list_was_read() {
        let r = Rig::new("adopt-replaced");
        r.run_value(Scope::User, N, r"C:\nope\old.exe");
        let e = r.entry(Scope::User, N);
        r.run_value(Scope::User, N, r"C:\nope\new-from-installer.exe"); // approval value untouched
        let err = adopt(&r.loc, &r.store, &e, app("One")).unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        assert_eq!(r.live(Scope::User, N).approved, None, "the replaced entry was not disabled");
        assert!(r.store.snapshot().cfg.apps.is_empty());
    }

    #[test]
    fn a_failed_save_stops_before_the_original_is_touched() {
        let r = Rig::new("adopt-save-fail");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        std::fs::create_dir_all(r.dir.join("startup.ini")).unwrap(); // the list can no longer be written
        let err = adopt(&r.loc, &r.store, &e, app("One")).unwrap_err();
        assert!(err.contains("Not changed"), "{err}");
        assert_eq!(r.live(Scope::User, N).approved, None, "the original was never disabled");
        assert!(r.store.snapshot().cfg.apps.is_empty());
    }

    #[test]
    fn a_failed_registry_write_rolls_the_record_back() {
        let r = Rig::new("adopt-reg-fail");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        FAIL_REGISTRY_WRITE.with(|f| f.set(true));
        let err = adopt(&r.loc, &r.store, &e, app("One")).unwrap_err();
        FAIL_REGISTRY_WRITE.with(|f| f.set(false));
        assert!(err.contains("was not changed"), "{err}");
        assert_eq!(r.live(Scope::User, N).approved, None);
        let snap = r.store.snapshot();
        assert!(snap.cfg.apps.is_empty(), "a failed take-over must not leave an enabled managed app next to the still-enabled original");
        assert!(!r.on_disk().contains("own.source") && !r.on_disk().contains("[app]"));
    }

    /// A failed durable write at any stage (temp write, flush, write-through
    /// replace) must stop the take-over BEFORE the registry is touched, and
    /// leave no half-saved record.
    #[test]
    fn a_failed_durable_write_never_changes_the_registry() {
        for stage in [1u8, 2, 3] {
            let r = Rig::new(&format!("adopt-durable-{stage}"));
            r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
            let e = r.entry(Scope::User, N);
            crate::osutil::DURABLE_FAIL.with(|f| f.set(stage));
            let res = adopt(&r.loc, &r.store, &e, app("One"));
            crate::osutil::DURABLE_FAIL.with(|f| f.set(0));
            let err = res.unwrap_err();
            assert!(err.contains("Not changed"), "stage {stage}: {err}");
            assert_eq!(r.live(Scope::User, N).approved, None, "stage {stage}: the original was never disabled");
            assert!(r.store.snapshot().cfg.apps.is_empty(), "stage {stage}");
            assert!(!r.on_disk().contains("own.source"), "stage {stage}: no ownership record on disk");
        }
    }

    /// The program is replaced right after the original was switched off (an
    /// installer at the worst moment): put the switch back, keep no managed app.
    #[test]
    fn a_source_replaced_during_the_takeover_is_put_back() {
        let r = Rig::new("adopt-replaced-late");
        r.run_value(Scope::User, N, r"C:\nope\old.exe");
        let e = r.entry(Scope::User, N);
        // the hook cannot borrow the Rig, so it rewrites the Run value through its own copy of the locations
        let hook_loc = Locations { hkcu: r.loc.hkcu, hklm: r.loc.hklm, user_dir: r.loc.user_dir.clone(), common_dir: r.loc.common_dir.clone() };
        AFTER_DISABLE.with(|c| {
            *c.borrow_mut() = Some(Box::new(move || {
                let k = reg::create(hook_loc.root(Scope::User), approved::RUN, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
                let v: Vec<u8> = crate::osutil::wide(r"C:\nope\new-from-installer.exe").iter().flat_map(|c| c.to_le_bytes()).collect();
                let n = crate::osutil::wide(N);
                unsafe {
                    assert_eq!(RegSetValueExW(k.0, crate::osutil::pcw(&n), None, REG_SZ, Some(&v)), windows::Win32::Foundation::ERROR_SUCCESS);
                }
            }));
        });
        let err = adopt(&r.loc, &r.store, &e, app("One")).unwrap_err();
        assert!(err.contains("replaced"), "{err}");
        assert_eq!(r.live(Scope::User, N).approved, None, "the switch was put back exactly");
        assert!(r.store.snapshot().cfg.apps.is_empty(), "no managed copy of a program that is no longer the one the user saw");
    }

    fn adopt_n(r: &Rig, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| {
                r.run_value(Scope::User, n, r"C:\nope\fixture.exe");
                let e = r.entry(Scope::User, n);
                adopt(&r.loc, &r.store, &e, app(n)).unwrap().app_id
            })
            .collect()
    }

    #[test]
    fn release_all_restores_every_owned_original() {
        let r = Rig::new("adopt-relall");
        let ids = adopt_n(&r, &["HayateStartupFixture R1", "HayateStartupFixture R2"]);
        let out = release_all(&r.loc, &r.store).unwrap();
        assert_eq!(out.restored, 2, "{out:?}");
        for n in ["HayateStartupFixture R1", "HayateStartupFixture R2"] {
            assert_eq!(r.live(Scope::User, n).approved, None, "{n} is back to its starting state");
        }
        let c = r.store.snapshot().cfg;
        assert!(ids.iter().all(|i| c.app(i).unwrap().own.is_none()));
    }

    /// Restore fails: the records stay (so a later unmanage/uninstall can still
    /// restore) and the caller is told to remove nothing.
    #[test]
    fn release_all_failure_keeps_every_record_and_aborts() {
        let r = Rig::new("adopt-relall-fail");
        let ids = adopt_n(&r, &["HayateStartupFixture F1", "HayateStartupFixture F2"]);
        FAIL_REGISTRY_WRITE.with(|f| f.set(true));
        let err = release_all(&r.loc, &r.store).unwrap_err();
        FAIL_REGISTRY_WRITE.with(|f| f.set(false));
        assert!(err.contains("Nothing was removed"), "{err}");
        let c = r.store.snapshot().cfg;
        assert!(ids.iter().all(|i| c.app(i).unwrap().own.as_ref().is_some_and(|o| o.state == OwnState::Owned)), "records kept until release is confirmed");
        for n in ["HayateStartupFixture F1", "HayateStartupFixture F2"] {
            assert_eq!(approved::state_of(r.live(Scope::User, n).approved.as_deref()), State::Disabled, "{n} untouched");
        }
        // and a later attempt that works restores both
        assert_eq!(release_all(&r.loc, &r.store).unwrap().restored, 2);
    }

    /// An all-users original without administrator rights cannot be restored:
    /// uninstall must stop, not skip it.
    #[test]
    fn release_all_denied_for_a_machine_entry_aborts() {
        let r = Rig::new("adopt-relall-denied");
        let id = adopt_n(&r, &["HayateStartupFixture D1"]).remove(0);
        // a record whose original lives under an all-users location the test key cannot be written through:
        // simulate the denial with the injected registry failure on that single entry
        FAIL_REGISTRY_WRITE.with(|f| f.set(true));
        let err = release_all(&r.loc, &r.store).unwrap_err();
        FAIL_REGISTRY_WRITE.with(|f| f.set(false));
        assert!(err.contains("D1"), "names the entry: {err}");
        assert!(r.store.snapshot().cfg.app(&id).unwrap().own.is_some());
    }

    #[test]
    fn release_all_leaves_an_externally_changed_entry_and_reports_it() {
        let r = Rig::new("adopt-relall-ext");
        adopt_n(&r, &["HayateStartupFixture E1"]);
        let id = EntryId::new(Kind::Run, Scope::User, View::Native, "HayateStartupFixture E1");
        let theirs = vec![6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        approved::write(&r.loc, &id, Some(&theirs)).unwrap();
        let out = release_all(&r.loc, &r.store).unwrap();
        assert_eq!(out.restored, 0);
        assert_eq!(out.left.len(), 1, "{out:?}");
        assert_eq!(r.live(Scope::User, "HayateStartupFixture E1").approved, Some(theirs), "not guessed at, not touched");
    }

    /// A list Hayate cannot fully read may hold ownership records: never "restore
    /// nothing and delete the backup".
    #[test]
    fn release_all_refuses_when_an_owned_section_was_dropped_or_the_list_is_locked() {
        let r = Rig::new("adopt-relall-lossy");
        adopt_n(&r, &["HayateStartupFixture L1"]);
        let mut ini = r.on_disk();
        ini.push_str("\n[app]\nid=zz\nname=lost\ncommand=x.exe\nown.source=run|user|64|k\nown.state=owned\nown.original=none\nown.written=ff\n");
        std::fs::write(r.dir.join("startup.ini"), ini).unwrap();
        r.store.load();
        let err = release_all(&r.loc, &r.store).unwrap_err();
        assert!(err.contains("could not be read") && err.contains("Nothing was removed"), "{err}");
        // a store that has not loaded
        let fresh = Store::new(r.dir.join("never-loaded.ini"));
        assert!(release_all(&r.loc, &fresh).unwrap_err().contains("Nothing was removed"));
    }

    #[test]
    fn reconcile_resolves_interrupted_takeovers_without_touching_the_registry() {
        let r = Rig::new("adopt-reconcile");
        for (i, n) in ["HayateStartupFixture P1", "HayateStartupFixture P2", "HayateStartupFixture P3"].iter().enumerate() {
            r.run_value(Scope::User, n, r"C:\nope\f.exe");
            let id = EntryId::new(Kind::Run, Scope::User, View::Native, *n);
            let written = approved::disabled_bytes(100 + i as u64);
            let o = Own { source: id.clone(), state: OwnState::Pending, original: None, written: written.clone(), raw_at_adopt: String::new() };
            r.store
                .update_sync(|c| {
                    let mut a = App::new(c.fresh_id(), format!("P{}", i + 1), r"C:\nope\f.exe".into());
                    a.own = Some(o.clone());
                    c.apps.push(a);
                })
                .unwrap();
            match i {
                0 => approved::write(&r.loc, &id, Some(&written)).unwrap(),                   // crashed after the write
                1 => {}                                                                         // crashed before the write
                _ => approved::write(&r.loc, &id, Some(&approved::enabled_bytes())).unwrap(), // someone else changed it
            }
        }
        let before: Vec<_> = (1..=3).map(|i| r.live(Scope::User, &format!("HayateStartupFixture P{i}")).approved).collect();
        let notes = reconcile(&r.loc, &r.store);
        assert_eq!(notes.len(), 3, "{notes:?}");
        let after: Vec<_> = (1..=3).map(|i| r.live(Scope::User, &format!("HayateStartupFixture P{i}")).approved).collect();
        assert_eq!(before, after, "reconcile writes only the list, never the registry");
        let c = r.store.snapshot().cfg;
        assert_eq!(c.apps[0].own.as_ref().unwrap().state, OwnState::Owned);
        assert_eq!(c.apps.len(), 2, "the take-over that never disabled anything is dropped with its managed entry");
        assert_eq!(c.apps[1].name, "P3");
        assert_eq!(c.apps[1].own.as_ref().unwrap().state, OwnState::Pending, "left for the user to look at");
    }

    #[test]
    fn remove_gives_the_original_back_first_and_unlinks_dependents() {
        let r = Rig::new("adopt-remove");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        let out = adopt(&r.loc, &r.store, &e, app("One")).unwrap();
        let first = out.app_id.clone();
        r.store
            .update_sync(|c| {
                let mut b = App::new(c.fresh_id(), "Two".into(), r"C:\nope\two.exe".into());
                b.after = Some(first.clone());
                c.apps.push(b);
            })
            .unwrap();
        assert_eq!(remove(&r.loc, &r.store, &first).unwrap(), Some(Released::Restored));
        assert_eq!(r.live(Scope::User, N).approved, None);
        let c = r.store.snapshot().cfg;
        assert_eq!(c.apps.len(), 1);
        assert_eq!(c.apps[0].after, None);
    }

    #[test]
    fn remove_keeps_the_entry_when_the_original_cannot_be_restored() {
        let r = Rig::new("adopt-remove-fail");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        let out = adopt(&r.loc, &r.store, &e, app("One")).unwrap();
        FAIL_REGISTRY_WRITE.with(|f| f.set(true));
        assert!(remove(&r.loc, &r.store, &out.app_id).is_err());
        FAIL_REGISTRY_WRITE.with(|f| f.set(false));
        assert!(r.store.snapshot().cfg.app(&out.app_id).unwrap().own.is_some(), "still owned: the user can retry");
        assert_eq!(approved::state_of(r.live(Scope::User, N).approved.as_deref()), State::Disabled);
    }

    #[test]
    fn a_stale_list_blocks_both_adopt_and_toggle() {
        let r = Rig::new("adopt-stale");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        approved::write(&r.loc, &e.id, Some(&approved::disabled_bytes(9))).unwrap(); // changed after the list was read
        assert!(adopt(&r.loc, &r.store, &e, app("One")).unwrap_err().contains("changed since"));
        assert!(set_enabled(&r.loc, &e, false, &r.store).unwrap_err().contains("changed since"));
        assert_eq!(r.live(Scope::User, N).approved, Some(approved::disabled_bytes(9)));
        assert!(r.store.snapshot().cfg.apps.is_empty());
    }

    #[test]
    fn ordinary_toggle_writes_only_the_approval_value() {
        let r = Rig::new("toggle");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let e = r.entry(Scope::User, N);
        set_enabled(&r.loc, &e, false, &r.store).unwrap();
        assert_eq!(r.entry(Scope::User, N).state(), State::Disabled);
        set_enabled(&r.loc, &r.entry(Scope::User, N), true, &r.store).unwrap();
        let live = r.live(Scope::User, N);
        assert!(live.present);
        assert_eq!(approved::state_of(live.approved.as_deref()), State::Enabled);
        // asking for the state it already has changes nothing
        set_enabled(&r.loc, &r.entry(Scope::User, N), true, &r.store).unwrap();
    }

    #[test]
    fn all_users_entries_are_changed_in_the_machine_layer_and_refuse_a_hidden_user_override() {
        let r = Rig::new("toggle-machine");
        r.run_value(Scope::AllUsers, N, r"C:\nope\machine.exe");
        let e = r.entry(Scope::AllUsers, N);
        set_enabled(&r.loc, &e, false, &r.store).unwrap();
        assert_eq!(approved::state_of(r.live(Scope::AllUsers, N).approved.as_deref()), State::Disabled);
        let uid = EntryId::new(Kind::Run, Scope::User, View::Native, N);
        approved::write(&r.loc, &uid, Some(&approved::enabled_bytes())).unwrap();
        let e = r.entry(Scope::AllUsers, N);
        assert!(set_enabled(&r.loc, &e, true, &r.store).unwrap_err().contains("own startup setting"));
        assert!(preview(&r.loc, &e, &r.store).blocked.is_some());
    }

    #[test]
    fn a_machine_entry_without_rights_says_so_before_changing_anything() {
        if crate::osutil::is_elevated() {
            return; // an elevated test run really can write HKLM: nothing to prove here
        }
        // the REAL hives, read-only probing: writable() only tries to open the key
        let loc = Locations { hkcu: HKEY_CURRENT_USER, hklm: HKEY_LOCAL_MACHINE, user_dir: PathBuf::new(), common_dir: PathBuf::new() };
        let id = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, "HayateStartupFixture DoesNotExist");
        let e = Entry { id: id.clone(), raw: String::new(), command: String::new(), target: None, icon: String::new(), live: Live::new(true, None, None), is_hayate: false, sig: None };
        let store = Store::new(std::env::temp_dir().join("HayateStartupFixture-nowrite.ini"));
        // the entry does not exist, so this stops at "no longer exists"; the rights pre-check is the same call chain
        assert!(set_enabled(&loc, &e, false, &store).is_err());
        assert!(can_write(&loc, &id).unwrap_err().contains("administrator"));
    }

    // ---- all-users records and their administrator-protected copy (`proof`) ----

    fn writes() -> Vec<EntryId> {
        WRITES.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }

    /// What an unelevated process of the user can do: put any record it likes
    /// into startup.ini (here through the store, then re-read from disk).
    fn forge(r: &Rig, id: &EntryId, written: Vec<u8>, original: Option<Vec<u8>>) -> String {
        let mut app_id = String::new();
        r.store
            .update_sync(|c| {
                let fid = c.fresh_id();
                let mut a = App::new(fid.clone(), format!("Forged {fid}"), r"C:\nope\x.exe".into());
                a.own = Some(Own { source: id.clone(), state: OwnState::Owned, original: original.clone(), written: written.clone(), raw_at_adopt: String::new() });
                app_id = a.id.clone();
                c.apps.push(a);
            })
            .unwrap();
        assert!(r.on_disk().contains("own.source=run|all|64|"), "the forged record is on disk");
        r.store.load();
        app_id
    }

    const M: &str = "HayateStartupFixture Machine";

    /// The P2 forgery: an all-users entry the admin disabled (or enabled), and a
    /// record claiming Hayate wrote exactly that value with the other state as
    /// "original". However it is given back (uninstall or unmanage), and even
    /// with administrator rights, no StartupApproved value is written.
    #[test]
    fn a_forged_all_users_record_never_writes_the_machine_value() {
        let r = Rig::new("forged-machine");
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let id = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, M);
        for (live, original) in [(approved::disabled_bytes(1234), Some(approved::enabled_bytes())), (approved::enabled_bytes(), Some(approved::disabled_bytes(1)))] {
            approved::write(&r.loc, &id, Some(&live)).unwrap();
            // uninstall (headless --startup-release and Settings > Uninstall both use release_all)
            forge(&r, &id, live.clone(), original.clone());
            writes();
            let out = release_all(&r.loc, &r.store).unwrap();
            assert!(writes().is_empty(), "no StartupApproved write was even attempted");
            assert_eq!((out.restored, out.left.len(), out.skipped.len()), (0, 0, 1), "{out:?}");
            assert!(out.skipped[0].contains("could not confirm"), "{out:?}");
            assert_eq!(r.live(Scope::AllUsers, M).approved, Some(live.clone()), "the admin's choice stands");
            assert!(r.store.snapshot().cfg.apps.iter().all(|a| a.own.is_none()), "the untrusted record is dropped");
            // unmanage / remove from the Startup tab
            let app_id = forge(&r, &id, live.clone(), original.clone());
            let rel = release(&r.loc, &r.store, &app_id).unwrap();
            assert!(matches!(rel, Released::ChangedSince(ref w) if w.contains("Task Manager")), "{rel:?}");
            let app_id = forge(&r, &id, live.clone(), original.clone());
            assert!(remove(&r.loc, &r.store, &app_id).is_ok());
            assert!(writes().is_empty());
            assert_eq!(r.live(Scope::AllUsers, M).approved, Some(live.clone()));
        }
        // a proof key the unelevated user made itself, holding exactly the right bytes, proves nothing
        if !crate::osutil::is_elevated() {
            let live = approved::disabled_bytes(1234);
            approved::write(&r.loc, &id, Some(&live)).unwrap();
            let k = reg::create(r.loc.hklm, super::super::proof::KEY, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
            let mut v = live.clone();
            v.extend_from_slice(&approved::enabled_bytes());
            reg::set_binary(&k, &EntryId::new(Kind::Run, Scope::AllUsers, View::Native, M.to_lowercase()).encode(), &v).unwrap();
            forge(&r, &id, live.clone(), Some(approved::enabled_bytes()));
            writes();
            let out = release_all(&r.loc, &r.store).unwrap();
            assert!(writes().is_empty() && out.skipped.len() == 1, "{out:?}");
            assert!(out.skipped[0].contains("not administrator-protected"), "{out:?}");
            assert_eq!(r.live(Scope::AllUsers, M).approved, Some(live));
        }
    }

    /// The secondary effect: the same forged record must not stop a per-user
    /// uninstall (before the fix the missing rights for HKLM aborted it). Any
    /// write at all would fail here, so only "never tried" gets through; the
    /// real per-user entry next to it is still given back.
    #[test]
    fn an_untrusted_record_never_aborts_the_uninstall() {
        let r = Rig::new("forged-uninstall");
        adopt_n(&r, &["HayateStartupFixture U1"]);
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let id = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, M);
        approved::write(&r.loc, &id, Some(&approved::disabled_bytes(55))).unwrap();
        forge(&r, &id, approved::disabled_bytes(55), None);
        // an unreadable all-users record (forged or corrupt) does not block it either...
        let mut ini = r.on_disk();
        ini.push_str("\n[app]\nid=zz\nname=lost\ncommand=x.exe\nown.source=run|all|64|k\nown.state=owned\nown.original=none\nown.written=ff\n");
        std::fs::write(r.dir.join("startup.ini"), ini).unwrap();
        r.store.load();
        assert_eq!(r.store.snapshot().dropped_owned, 0);
        FAIL_REGISTRY_WRITE.with(|f| f.set(true));
        let err = release_all(&r.loc, &r.store).unwrap_err();
        FAIL_REGISTRY_WRITE.with(|f| f.set(false));
        assert!(err.contains("U1") && !err.contains("Forged"), "only the real entry's write failed: {err}");
        assert!(!writes().contains(&id), "the all-users value was never tried");
        forge(&r, &id, approved::disabled_bytes(55), None); // the failed run dropped it; put it back
        writes();
        let out = release_all(&r.loc, &r.store).unwrap();
        assert_eq!((out.restored, out.skipped.len()), (1, 1), "{out:?}");
        assert_eq!(writes(), vec![EntryId::new(Kind::Run, Scope::User, View::Native, "HayateStartupFixture U1")], "the per-user restore is unchanged");
        assert_eq!(r.live(Scope::User, "HayateStartupFixture U1").approved, None);
        assert_eq!(r.live(Scope::AllUsers, M).approved, Some(approved::disabled_bytes(55)));
    }

    /// A real all-users take-over (as an elevated Hayate makes it) writes the
    /// protected copy before the original is touched, is given back through
    /// it, and a record edited afterwards no longer matches it.
    #[test]
    fn an_all_users_takeover_is_given_back_only_through_its_protected_copy() {
        let r = Rig::new("machine-proof");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(true)); // the scratch key stands in for the admin-only one
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let e = r.entry(Scope::AllUsers, M);
        assert!(preview(&r.loc, &e, &r.store).lines.iter().any(|l| l.contains(super::super::proof::KEY)));
        let out = adopt(&r.loc, &r.store, &e, app("Machine")).unwrap();
        let own = r.store.snapshot().cfg.app(&out.app_id).unwrap().own.clone().unwrap();
        assert_eq!(super::super::proof::check(&r.loc, &e.id, &own.written, own.original.as_deref()), Ok(()));
        assert_eq!(release(&r.loc, &r.store, &out.app_id).unwrap(), Released::Restored);
        assert_eq!(r.live(Scope::AllUsers, M).approved, None, "exactly the starting state");
        assert!(super::super::proof::check(&r.loc, &e.id, &own.written, own.original.as_deref()).is_err(), "the copy goes with the record");

        // take it over again, then an unelevated process edits the record's original
        let e = r.entry(Scope::AllUsers, M);
        let out = adopt(&r.loc, &r.store, &e, app("Machine again")).unwrap();
        let disabled = r.live(Scope::AllUsers, M).approved;
        let id = out.app_id.clone();
        r.store.update_sync(|c| c.apps.iter_mut().find(|a| a.id == id).unwrap().own.as_mut().unwrap().original = Some(approved::disabled_bytes(9))).unwrap();
        writes();
        let all = release_all(&r.loc, &r.store).unwrap();
        assert!(writes().is_empty() && all.skipped.len() == 1, "{all:?}");
        assert!(all.skipped[0].contains("says something else"), "{all:?}");
        assert_eq!(r.live(Scope::AllUsers, M).approved, disabled);
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(false));
    }

    /// Without a protected copy Hayate could never give an all-users entry back,
    /// so it does not take it over (an unelevated test run cannot create the
    /// admin-owned key, which stands in for every way that save can fail).
    #[test]
    fn an_all_users_takeover_stops_before_the_original_when_its_copy_cannot_be_saved() {
        if crate::osutil::is_elevated() {
            return;
        }
        let r = Rig::new("machine-noproof");
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let e = r.entry(Scope::AllUsers, M);
        writes();
        let err = adopt(&r.loc, &r.store, &e, app("Machine")).unwrap_err();
        assert!(err.contains("Not changed") && err.contains("administrator-protected"), "{err}");
        assert!(writes().is_empty(), "the original was never touched");
        assert_eq!(r.live(Scope::AllUsers, M).approved, None);
        assert!(r.store.snapshot().cfg.apps.is_empty());
    }

    fn proven(r: &Rig, id: &EntryId, o: &Own) -> bool {
        super::super::proof::check(&r.loc, id, &o.written, o.original.as_deref()).is_ok()
    }

    fn pending(r: &Rig, id: &EntryId, written: Vec<u8>, original: Option<Vec<u8>>) -> Own {
        let o = Own { source: id.clone(), state: OwnState::Pending, original, written, raw_at_adopt: String::new() };
        r.store
            .update_sync(|c| {
                let mut a = App::new(c.fresh_id(), "Pending".into(), r"C:\nope\machine.exe".into());
                a.own = Some(o.clone());
                c.apps.push(a);
            })
            .unwrap();
        o
    }

    /// A take-over that never disabled anything is dropped by reconcile; its
    /// protected copy goes with it, but only the copy of exactly that record.
    #[test]
    fn a_dropped_pending_all_users_record_takes_only_its_own_copy_with_it() {
        let r = Rig::new("reconcile-proof");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(true));
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let id = EntryId::new(Kind::Run, Scope::AllUsers, View::Native, M);
        // crashed after the copy was saved, before the original was disabled
        let o = pending(&r, &id, approved::disabled_bytes(70), None);
        super::super::proof::save(&r.loc, &id, &o.written, None).unwrap();
        writes();
        reconcile(&r.loc, &r.store);
        assert!(r.store.snapshot().cfg.apps.is_empty(), "dropped");
        assert!(!proven(&r, &id, &o), "the copy went with the record");
        assert!(writes().is_empty() && r.live(Scope::AllUsers, M).approved.is_none(), "reconcile still never writes a StartupApproved value");

        // a forged pending record for the same entry must not erase the real record's copy
        let real = Own { source: id.clone(), state: OwnState::Owned, original: None, written: approved::disabled_bytes(71), raw_at_adopt: String::new() };
        super::super::proof::save(&r.loc, &id, &real.written, None).unwrap();
        pending(&r, &id, approved::disabled_bytes(72), None);
        reconcile(&r.loc, &r.store);
        assert!(r.store.snapshot().cfg.apps.is_empty());
        assert!(proven(&r, &id, &real), "another record's copy stays");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(false));
    }

    /// Given back after somebody changed the value: the record is dropped and,
    /// when it proves exactly that record, so is the copy.
    #[test]
    fn an_all_users_entry_changed_outside_hayate_takes_its_own_copy_with_it() {
        let r = Rig::new("changed-proof");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(true));
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let e = r.entry(Scope::AllUsers, M);
        let out = adopt(&r.loc, &r.store, &e, app("Machine")).unwrap();
        let own = r.store.snapshot().cfg.app(&out.app_id).unwrap().own.clone().unwrap();
        assert!(proven(&r, &e.id, &own));
        let theirs = vec![6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        approved::write(&r.loc, &e.id, Some(&theirs)).unwrap(); // the admin turned it back on in Task Manager
        writes();
        assert!(matches!(release(&r.loc, &r.store, &out.app_id).unwrap(), Released::ChangedSince(_)));
        assert!(writes().is_empty(), "nothing written");
        assert_eq!(r.live(Scope::AllUsers, M).approved, Some(theirs.clone()));
        assert!(!proven(&r, &e.id, &own), "the copy went with the record");

        // a forged record that does not match the copy leaves the copy alone
        let real = Own { source: e.id.clone(), state: OwnState::Owned, original: None, written: approved::disabled_bytes(81), raw_at_adopt: String::new() };
        super::super::proof::save(&r.loc, &e.id, &real.written, None).unwrap();
        let fid = forge(&r, &e.id, approved::disabled_bytes(82), None);
        assert!(matches!(release(&r.loc, &r.store, &fid).unwrap(), Released::ChangedSince(_)));
        assert!(proven(&r, &e.id, &real), "another record's copy stays");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(false));
    }

    /// An uninstall that gave every entry back also removes the protected key
    /// once it is empty (and SOFTWARE\Hayate when that is empty too).
    #[test]
    fn release_all_removes_the_empty_protected_key() {
        let r = Rig::new("relall-tidy");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(true));
        r.run_value(Scope::AllUsers, M, r"C:\nope\machine.exe");
        let e = r.entry(Scope::AllUsers, M);
        adopt(&r.loc, &r.store, &e, app("Machine")).unwrap();
        assert!(reg::open(r.loc.hklm, super::super::proof::KEY, KEY_READ).is_ok());
        let out = release_all(&r.loc, &r.store).unwrap();
        assert_eq!(out.restored, 1, "{out:?}");
        assert!(matches!(reg::open(r.loc.hklm, super::super::proof::KEY, KEY_READ), Err(e) if e.not_found()), "the empty key is gone");
        assert!(matches!(reg::open(r.loc.hklm, r"SOFTWARE\Hayate", KEY_READ), Err(e) if e.not_found()), "and its empty parent");
        super::super::proof::TRUST_ANY_KEY.with(|t| t.set(false));
    }

    #[test]
    fn the_uninstall_summary_names_skipped_entries_plainly() {
        let mut r = ReleasedAll { restored: 2, left: vec!["a".into()], skipped: vec![] };
        assert_eq!(r.summary(), "2 given back, 1 left as changed outside Hayate");
        r.skipped.push("x".into());
        assert_eq!(r.summary(), "2 given back, 1 left as changed outside Hayate; 1 all-users entry left disabled: re-enable in Task Manager");
        r.skipped.push("y".into());
        assert!(r.summary().ends_with("; 2 all-users entries left disabled: re-enable in Task Manager"));
    }

    #[test]
    fn hayates_own_autostart_cannot_be_taken_over() {
        let r = Rig::new("self");
        r.run_value(Scope::User, N, r"C:\nope\fixture.exe");
        let mut e = r.entry(Scope::User, N);
        e.is_hayate = true;
        let p = preview(&r.loc, &e, &r.store);
        assert!(p.blocked.unwrap().contains("Start with Windows"));
        assert!(adopt(&r.loc, &r.store, &e, app("Self")).is_err());
    }
}
