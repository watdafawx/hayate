# Startup tab: Launchpad parity

Source of truth for what the Startup tab does compared with
[Launchpad](https://github.com/watdafawx/launchpad) (read-only reference at `C:\src\launchpad`, README and
source; Launchpad was never launched, edited or configured for this work).

**This is not full parity.** Every user-facing Launchpad feature is listed below with a status. Items marked
*deferred* or *different* are deliberate and say why. Anything not proven on a real machine is marked *unverified*.

Status words: **existing** (Hayate already had it) · **reuse** (existing Hayate code reused) · **implemented**
(new in this branch) · **different** (done on purpose another way) · **deferred** (not done, reason given).

## Feature map

| Launchpad feature | Status | Where / notes |
|---|---|---|
| List Windows startup items (HKCU/HKLM Run, Run32, user + common Startup folders) | implemented | `startup/scan.rs`. Stable identity `kind\|scope\|view\|key` (`startup/id.rs`). A registry read that fails makes the entry (or the whole source) *Unknown* and adds a problem line; it is never shown as present and enabled. Startup-folder entries that are reparse points are listed by name only (source note), their target is not followed. Shortcut targets are parsed (COM) only by a Hayate that is provably **not** elevated, and on the I/O worker, never on the UI thread. |
| StartupApproved state (enabled / disabled, per entry) | implemented | `startup/approved.rs`. Only the canonical Windows shape is understood: `REG_BINARY`, 12 bytes, first byte one of 2/3/6/7, bytes 1..4 zero, FILETIME kept. Any other type, length or first byte is *Unknown* and is never written over. A missing value is Windows' default (enabled). Whitelist, not blacklist: a state is only known when verified. |
| Per-user vs all-users approval layers | implemented | When the two layers disagree the state is *Unknown*, in the list and in the write refusal (same rule, same code). Windows' exact precedence is not documented; Hayate does not guess. |
| Turn a Windows entry on / off | implemented | Explicit button only. HKLM / common entries follow Windows rights: no automatic UAC, no ACL change. Hayate offers the existing *Restart as administrator* flow instead. Refused over an *Unknown* value or an entry that changed since it was listed. |
| "Manage" an item (take it over, disable the original) | implemented | `startup/adopt.rs`. Preview first; nothing is written until *Save and manage*. See "Take-over" below. |
| Give the original back (unmanage / uninstall) | implemented | Restores **only** the exact bytes Hayate wrote, and only while the value is still what Hayate wrote. If anything else changed it, Hayate refuses and says so. An all-users entry is given back only through its administrator-protected copy of the record (see "Take-over"). Uninstall gives back every taken-over original first (not plain on/off toggles), see "Uninstall". |
| Managed app editor (command, `.lnk`, `shell:AppsFolder\…`, `%VARS%`) | implemented | `ui/startup.rs` editor, `startup/model.rs` validation. Add app from scratch is also possible (Launchpad needs an existing Windows item first). An unquoted path is only split at a known file boundary; an ambiguous one (`C:\a.exe b\x.exe`) is refused, never guessed. `.bat` / `.cmd` go through the shell (`ShellExecute open`, arguments separate), never a `cmd /c` string. |
| After (start only after another app started or was skipped) | implemented | `startup/sched.rs`. Cycles, missing targets and duplicates are rejected on save and on load. |
| Delay (seconds, counted from *After* or from sign-in) | implemented | 0 to 1 h, bounded. |
| CPU below % (3 s steady, give up after 90 s) | implemented | Bounded idle gate; after 90 s the app starts anyway. No poller runs when no entry uses it. |
| Admin checkbox | **different** | Launchpad starts Admin entries at sign-in. Hayate does **not**: an Admin entry only starts from an explicit *Run now* / *Run all now*. See "Elevation". Hayate does not promise a UAC prompt: Windows policy decides whether `runas` asks. *Unverified on rig*: starting an Admin entry needs elevation, which tests must not trigger. Covered by unit tests of the launch policy only. |
| Process name (skip when already running) | implemented | Blank = derived from the command. Matched case-insensitively. A launch that Windows has not answered yet keeps its process name reserved (see "Launch bounds"). |
| Per-entry on / off, remove | implemented | Remove asks for a second click. Removing an app gives its original back first (or keeps the app if that failed) and clears `After` on apps that depended on it. |
| Run now (one entry / all) with status and log | implemented | `Run now`, `Run all now`, `Cancel`; *Last run* column and `startup-last-run.log` in the Hayate data dir. Run now / Run all now are disabled while a launch is unanswered. |
| Start the list at sign-in | implemented, **opt-in** | *Run at sign-in* switch, default OFF. See "Lifecycle". |
| Import from Launchpad | implemented | Explicit file path, read-only preview, explicit selection, dual-launch warning. Entries land **off**. The file is read with a bound (max + 1 byte, before any allocation), a UTF-8 BOM is stripped by the real parser. Launchpad's own file and startup entries are never modified. |
| Launchpad `guard` (while resident, re-apply "original is off" if an installer switches it back on) | **deferred** | Launchpad does this by staying resident and re-applying at start and on config change. Nothing in Hayate does that: no resident re-apply, no watcher, no poll. What exists instead: a managed original that is enabled again shows as such after *Refresh*, and nothing is re-written without a click. If an installer switches an original back on, the app can start twice until the user presses Unmanage / Manage again. |
| Tray icon, tray menu | deferred | Hayate already has its own resident mode and tray. A second tray identity would be a second autorun surface. |
| Installer / `--install` / `--uninstall` | existing | Hayate's own installer (Settings) and `install.ps1`. Both now give owned startup entries back before removing anything, see "Uninstall". |
| Auto-update of the manager | existing | Hayate's own updater. It never re-runs the startup list: the private sign-in flag is stripped on update relaunch. |
| MCP server (`get_setup`, `list_startup_items`, `suggest_changes`, …) | **deferred** | Not implemented, not partly implemented. Hayate has no MCP server and no suggestion store, and an assistant-writable list that a logon runner executes would be a trust-boundary hole. |
| Suggestions UI (Apply / Dismiss) | deferred | Same reason as MCP. |
| Launchpad's separate Settings window process | different | Startup is a normal Hayate tab. |
| Task Manager "Startup impact" | different | Not shown. Hayate does not have a trustworthy source, and showing 0 would be wrong. |

## Lifecycle (the critical part)

* **Default OFF.** `manage=0` in `startup.ini`. A fresh Hayate data dir has no managed list and no switch on.
* **Never automatic.** Opening the tab only *reads*: it scans Windows entries and loads the list. It does not import,
  adopt, disable or launch anything. *Usual place* in the import dialog only fills the text box; the file is read
  only when *Load* is pressed.
* **Runs only at sign-in, once, and only from Hayate's own autorun.** `startup/run.rs::decide` allows the logon run
  only when all hold:
  1. the switch is ON and the list is valid with at least one runnable entry;
  2. Hayate was started with `--resident` **and** the private flag `--startup-logon` (what its own *Start with
     Windows* shortcut carries once the switch was saved ON);
  3. that shortcut (`Hayate.lnk` in the user Startup folder) exists, starts a Hayate exe, is **enabled** in Windows
     and carries the flag (read by a non-elevated Hayate only);
  4. Hayate is provably **not** elevated (an unknown elevation answer is a refusal, not "normal");
  5. Windows reports this logon's start time and it is ≤ 600 s ago;
  6. the atomic once-per-logon claim is won (below).
  A manual `--resident`, an update relaunch, *Restart as administrator*, or a resident restart carries no flag and
  never runs the list, even inside the ten-minute window (unit test: manual start inside the window does not run).
  The old rule (`--resident` + logon age) was not provenance and is gone.
* **The flag is not a secret.** Anyone who can start `hayate.exe --resident --startup-logon` by hand can try to run the
  list. What bounds it: the claim allows one run per logon, the shortcut must be enabled and flagged, and an
  elevated Hayate never runs the list. The flag only appears in Hayate's own shortcut after the user turns the switch
  ON; turning it OFF takes it out again (other shortcut arguments are kept, unrelated ones are never touched, and a
  failed save puts the old bytes back and switches the list back OFF). It is stripped from relaunch arguments
  (`update::logic::PRIVATE_FLAGS`). A reinstall rewrites the shortcut and drops the flag, which is safe: the list
  then simply does not run until the switch is saved ON again. An elevated Hayate does not edit the shortcut.
* **Claim = one run per logon.** `startup-run-<fnv(identity)>.claim`, created with `create_new` (atomic). The
  identity is the user SID, the token's logon-session LUID, the session id and the session's logon time, so two
  logons never share one. `Won` is only returned after the claim content was written and flushed; a claim that
  cannot be recorded is removed again and reported as a failure (no run). Claims of **other** logons are never
  deleted on a guess: only when Windows positively says that session no longer exists, or exists with a different
  logon time. An unknown answer, an unreadable file or a live session of the same user keeps the claim, so two
  concurrent logons of one user cannot rerun each other.
* **"Start with Windows"** (the existing Hayate autostart) is only *toggled* when Hayate already has its own Startup
  entry. The only change the switch makes to Hayate's own shortcut is the one private argument above, and only on
  the explicit *Run at sign-in* click. The tab creates the shortcut in one place only, the inline offer below.
* **Hint and offer when the list could not run at sign-in.** The switch is ON (or the user just pressed ON and it
  did not stick), but Hayate's own Start with Windows shortcut is missing, switched off in Windows, or lacks the
  private flag. Then (and only then; never on tab open for an OFF switch) the status line says plainly *"The list
  will not run at sign-in: <why>"* and an inline, non-modal row appears under *Start with Windows*: *"Run at
  sign-in needs Hayate to start with Windows (hidden in the tray)."* with one button, **Start Hayate with Windows**.
  When everything is right and the switch is ON, the status line says the list runs once at the next sign-in,
  hidden, and nothing starts now. Nothing is enabled automatically; the button is the consent.
  * What the click does (`own::make_ready`, on the I/O worker, which reads the shortcut again first): no
    shortcut -> `setup::enable_autostart()` writes it with the **same write the installer's Start with Windows
    option uses** (`setup::write_autostart`: `--resident`, plain, never elevated, no scheduled task, no `Run`
    value); disabled in Windows -> StartupApproved back to enabled (what Task Manager does, never over a value of
    an unknown shape); lacks the flag and the list is ON -> `own::set_flag(true)` (same function the switch uses,
    exact-bytes rollback). If the list is OFF the flag is **not** added (it only appears when the switch is ON),
    and the message says Run at sign-in is still off. Neither switch is ever turned on by it.
  * Never offered (text only, no button): an elevated Hayate or one that cannot tell its elevation (it does not
    read or write the shortcut), a shortcut that does not start Hayate or not hidden (`--resident`), an unreadable
    one, a StartupApproved value of a shape Hayate does not recognise (text: "change it in Task Manager"; not
    confused with "switched off in Windows", which does get the button), and **any copy that is not the per-user
    installed one** (portable or per-machine) **for every gap**: a portable copy would write a shortcut to wherever
    it sits, or switch on / flag a shortcut that points at the old installed exe. The text says to open the
    installed Hayate or install from Settings first.
  * The gates are inside `own::make_ready` itself, checked first, before anything is read or written: per-user
    installed copy only (`setup::per_user_installed()`), and provably not elevated (`elevation() == Some(false)`).
    `setup::enable_autostart` repeats both for the one write. The UI no longer carries its own copy of the check.
  * State comes from the Startup tab's own scan (already off the UI thread, re-run after every job and by
    Refresh); no new polling. Until the first scan arrived nothing is shown, never a false "missing".
  * `decide()` provenance rules are unchanged. *Unverified:* a real sign-in with the offered shortcut.
* **No second autorun.** No service, scheduled task, IFEO or `Run` value is created by this work.
* Launchpad is **not** disabled behind its back. The import dialog warns that both may start the same programs.

## Elevation

* **Elevation is queried once and fails closed.** `osutil::elevation()` returns `Some(true)`, `Some(false)` or
  `None` (query failed). Everything on the startup launch path treats `None` as a refusal; nothing uses
  `is_elevated()` (which maps `None` to "normal") as a gate.
* Non-admin entries start with the **normal interactive user token**: a non-elevated Hayate starts a plain child;
  an elevated Hayate asks Explorer (already unelevated), with a verified fallback (same user, same session,
  non-elevated shell token) that **fails closed**. `actions::restart` checks that fallback before it terminates
  the target, so a refusal no longer kills the program and then fails to relaunch it.
* **Admin entries are explicit only.** They start with `runas` from *Run now* or *Run all now*, from a
  **non-elevated** Hayate. The automatic sign-in run skips them (status "press Run now") and everything that
  waits (`After`) for one is blocked, with the reason shown. Reason: with `ConsentPromptBehaviorAdmin=0` (or an
  equivalent policy) `runas` can elevate silently, which would turn the sign-in run into silent elevated
  execution of a per-user config. Hayate never says "a UAC prompt every time": whether Windows asks is Windows
  policy.
* An **elevated Hayate never starts Admin entries and never runs per-user config high**, and it never parses
  user-writable shortcuts (it lists them by name only).
* Rig run (non-elevated Hayate): fixture process owner was the normal user, parent = Hayate (expected for a
  non-elevated Hayate). The elevated-Hayate / Explorer path is unit-tested only (*unverified on rig*).

## Take-over (adopt)

Order: **durable *Pending* record → re-read the source and compare → disable the original → read back →
re-compare the source → *Owned* record**. Any failure rolls back what Hayate wrote and drops the half-made managed
app, so a failed take-over can never leave an enabled managed app next to an enabled original.

* The preview captures the source (type + raw bytes, or the shortcut file's bytes / hash). The commit re-reads it
  and refuses when it changed (an installer replaced it between preview and Save). The same comparison runs after
  the disable. `raw_at_adopt` is validated, not trusted.
* Nothing is written to the registry unless the durable *Pending* record was written, flushed, replaced and **read
  back** from disk first. If that fails, the registry is never touched (tests inject a failure at the temp write,
  the flush and the replace).
* **Not an absolute guarantee.** Windows offers no compare-and-swap on a registry value. Between the last re-check
  and the write there is a small window in which another program can still change the entry. Hayate keeps the window
  to a few calls, writes only the exact bytes it records, restores only those exact bytes, and never overwrites a
  value that no longer matches what it wrote.
* A *Pending* record found at the next start is reconciled **without writing the registry** (decided from what is
  there now). A managed app whose take-over was never confirmed is excluded from every run and shows
  "take-over unfinished" until it is settled.
* Machine-scope entries need Windows administrator rights: without them the take-over and the give-back are
  refused, nothing is written.
* **All-users records need an administrator-protected copy** (`startup/proof.rs`). `startup.ini` lives in the
  user-writable data dir, so any unelevated process of the user could forge an ownership record; an elevated Hayate
  must not write an HKLM value on the strength of that file alone. Taking over an all-users entry therefore also
  writes the record (written + original bytes) to `HKLM\SOFTWARE\Hayate\StartupOwnership`, a key Hayate creates
  with a protected access list (owner Administrators; SYSTEM and Administrators full, Users read). That copy is
  saved before the original is touched; if it cannot be saved, the take-over stops with nothing changed. Giving an
  all-users entry back (Unmanage, Remove, Uninstall, `--startup-release`) writes only when the key is owned by
  SYSTEM or Administrators, grants write to nobody else (checked on the open handle), and holds exactly the
  user-store record. Otherwise nothing is written, the record is dropped, the log and the message say so, and the
  user is pointed at Task Manager. An existing key with a wider access list is never repaired, it just proves
  nothing. Per-user (HKCU) records need no copy: whoever can forge them can write HKCU anyway.

## Uninstall

A destructive uninstall must not leave Windows entries disabled by a program that no longer exists. Every path
gives back every entry Hayate **took over** (*Manage in Hayate*, which keeps an ownership record) first, and removes
**nothing** if one of those cannot be given back. An entry that was only switched on or off with the plain
enable/disable toggle (`adopt::set_enabled`) has no ownership record and is **never** put back: it stays the way the
user left it, as after the same click in Task Manager. An all-users record without its administrator-protected copy
(see "Take-over") is skipped with a logged warning and never stops the uninstall. Its entry stays disabled, and the
summary line in `hayate.log` says so in plain words: "N all-users entries left disabled: re-enable in Task Manager".

**Protected copies after an uninstall.** A copy in `HKLM\SOFTWARE\Hayate\StartupOwnership` is removed after a give-back
it just proved, and (from an elevated Hayate only, and only when it matches that exact record) when its record is
dropped because the entry was changed outside Hayate or the take-over never happened (reconcile). When an uninstall
gave everything back, an elevated Hayate also removes the `StartupOwnership` key once it holds no value or subkey at
all (another user's take-over keeps it), and then `SOFTWARE\Hayate` if that is empty too. What survives by design:
copies an unelevated Hayate could not remove (a per-user uninstall normally runs unelevated), copies whose record
was lost or edited, and the key itself whenever any value is left or the uninstall was not elevated. A stale copy
alone proves nothing: a give-back still needs the live value to be exactly the timestamped bytes Hayate wrote. They
can be deleted by hand (as administrator) once no Hayate of any user manages an all-users entry.

**Release note (builds before the 2026-11-18 forgery fix).** All-users take-overs made by a build before
`a69e214` have no protected copy. Such a record is never given back (it is indistinguishable from a forged one):
Unmanage, Remove and Uninstall skip it, leave the entry disabled and say "re-enable in Task Manager". Per-user
take-overs are not affected. No published build had the Startup tab before that fix (the published `8206d31` and every older build do not), so
only development and test builds can have made such records.

| Path | Behaviour |
|---|---|
| Settings > Uninstall (`setup::uninstall`, per-user install) | Calls `release_all_owned()` before the running exe is renamed. On `Err` it returns before anything is removed; the exe, shortcuts and data stay, and the message says why. The data delete (*Also delete my data*) can only happen after that. |
| `hayate.exe --startup-release` (headless, no window) | Same function; exit code 0 = everything owned is back (or nothing was owned), 1 = something could not be given back. Result in `hayate.log`. |
| `install.ps1 -Uninstall` | If `startup.ini` has ownership records, runs `--startup-release` first, from the same exe the script already picks for the Jump List: **elevated runs execute only the per-machine exe, normal runs only the per-user exe** (an elevated run never executes the user-writable per-user exe). Non-zero exit, or no exe that may run it, stops the script with "Nothing was removed". It never elevates itself. |
| Per-machine uninstall of a per-user list | Needs a normal terminal first, as the script says. |

`release_all` refuses (and the caller removes nothing) when the list is not loaded, is locked, or lost an ownership
section of a per-user entry it could not read (`dropped_owned`, original text kept in `startup.ini.bak`). An
unreadable all-users section does not count: it could not be honored without its protected copy anyway. An entry somebody else changed
since is left exactly as it is and reported; it is not a failure (there is nothing of Hayate's to restore). Rollback
records are kept until the release is confirmed. Hayate's own data dir stays after a normal uninstall; the in-app
option *Also delete my data* removes it only after the release succeeded.

## Durability, and what is not claimed

* `startup.ini`, claim files and the take-over record are written with `osutil::write_durable`: temp file in the
  same folder, `sync_all` (FlushFileBuffers), then `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`, then read back.
  Writes are serialised by one lock that covers a whole registry + record operation, so an abandoned worker cannot
  race a newer write; `Store::update_sync` is the only way the list changes (the old fire-and-forget path that kept
  edits after a failure was removed).
* **No claim about the directory entry.** Hayate does not flush the containing folder. After a power loss at the
  worst instant the rename itself may not have reached the disk; then the old file is still there. For a take-over
  this means: the registry is only changed after the replaced file was read back, so a lost rename after that
  point is the one remaining case where an original could stay disabled without a record. This is very unlikely
  (WRITE_THROUGH plus read-back) and not impossible, and it is not claimed to be impossible.
* Entries in `startup.ini` that cannot be read are **not** silently dropped: the tab shows a warning with the
  count and names before the list is saved again, and the original text is kept in `startup.ini.bak` before the
  first rewrite. Backups are a last resort, not the notice.

## Launch bounds

* A launch is handed to a short-lived thread with a gate right before the OS call: *Cancel* before that point
  means the program is never started ("Cancelled"). After it, `ShellExecute` / `CreateProcess` cannot be recalled.
* If a launch has not answered after 120 s the row says **"Unconfirmed … may still start"**, not failed. The
  process name stays **reserved** and the launch stays in flight beyond the run: *Run now*, *Run all now* and a
  second run are refused, and a duplicate entry for the same program in the same run is blocked, until the OS
  call really returns. The late answer is logged and updates the row ("Started late" / "Failed"). If the call
  never returns the reservation stays until Hayate exits: that is the honest bound, not a bug.
* Two entries for the same program in one batch: the second waits while the first launch is pending, then
  re-checks a fresh process snapshot (case-insensitive) after it completed. A failed first launch lets the second
  try.
* *Cancel* reports how many programs were already handed to Windows and cannot be recalled.

## Deviations from Launchpad semantics

1. **A failed or cancelled dependency blocks its dependents.** Launchpad marks a dependency "done" whatever
   happened to it, so dependents still start. Hayate only treats *started* or *skipped (already running)* as
   satisfying *After*; dependents of a failed, cancelled, unconfirmed or Admin-skipped app are reported `Blocked`.
2. **A dependency that no longer exists is a validation error** (the list is refused until fixed). A dependency
   that exists but is switched off is not waited for, same as Launchpad.
3. **Waits are bounded and explicit.** Delay ≤ 1 h, idle gate 90 s then start anyway, launch 120 s (then
   *Unconfirmed*, see above). Launchpad's dependency wait gives up after 300 s and starts anyway; Hayate has no
   separate dependency timeout, because every dependency is itself bounded.
4. **Import never adopts originals.** Launchpad's `[app.startup]` source is shown as information only. The user
   adopts later with *Manage in Hayate…* (preview and Save first).
5. **Approval layers that disagree are *Unknown*, never guessed.** Windows' exact precedence is not documented.
   *Unverified* against Windows itself; the refusal is the safe side.
6. **Admin entries do not start at sign-in** (Launchpad's do). Explicit *Run now* only.
7. Import accepts a documented TOML **subset** (basic/literal strings, bools, numbers, `[[app]]`, `[app.startup]`).
   File ≤ 512 KB, UTF-8 (BOM stripped), ≤ 200 apps, fields length-checked. Unknown keys are ignored.

## Trust boundaries and bounds

* `startup.ini` and the import file are untrusted input: size (bounded read, never read-then-check), count, field
  length, control characters, delay and CPU ranges are all checked on load and again before launch.
* The UI shows a result message **only after** the job returns (no phantom "Saved"); a failed write rolls the
  in-memory list back. A change that is refused because another one is still saving does not start, does not mark
  an editor / importer as saving, and a finished job only closes the overlay that started that very job (job
  token), so another job finishing can never close an editor or discard an unsaved edit.
* Known Folders come from the shell API, not environment guesses.
* Launch uses a verified command line; shell items and `.bat` / `.cmd` go through the shell with separate
  arguments, never `cmd /c <string>`.
* Machine autorun entries and user-writable shortcuts are never executed by an elevated Hayate on behalf of the
  per-user list.

## API / security delta

* New module tree `src/startup/` (identity, scan, approved, model, sched, launch, store, adopt, run, import, own).
* `startup::own` reads / edits Hayate's own Startup shortcut (one private argument, rollback to exact bytes).
* `osutil`: `write_durable`, `read_bounded`, `elevation()` (tri-state, cached, no fallback).
* New helpers: `launch_as_user` (+ `TokenFacts`), `win::paste_text` (unchanged here), `Win32_System_RemoteDesktop`
  feature (WTS logon time and session liveness).
* New CLI: `hayate.exe --startup-release` (headless give-back for uninstall). New private flag
  `--startup-logon` (stripped on relaunch).
* `actions::restart` preflights the verified shell token before terminating the target.
* No new network access, services, tasks, drivers or privileges. Writes: `HKCU/HKLM …\StartupApproved\*` (only on
  explicit user action and only for the one entry), the Hayate data dir (`startup.ini`, `.bak`, `startup-last-run.log`,
  claim files), and the user's own `Hayate.lnk` (its private argument on the explicit switch; the file itself and its StartupApproved value only on the explicit *Start Hayate with Windows* click).
* Reviewers should look hardest at: `adopt.rs` (ordering, exact-bytes restore, `release_all`), `run.rs`
  (claim identity, provenance, in-flight reservation), `own.rs` (shortcut edit), `launch.rs` /
  `actions.rs::launch_as_user` (token policy), `import.rs` (parser limits), `install.ps1` (uninstall order).

## Proof

* Unit tests, each regression written to fail against the previous behaviour: uninstall release (restore all,
  failure keeps records, denied machine entry, externally changed entry, lossy / locked list); durable-write
  failure at temp write / flush / replace never changes the registry; source replaced during the take-over is put
  back; typed `StartupApproved` shapes (type, length, state); layer conflict is *Unknown*; claim identity,
  concurrent logons keep their claims, only positive proof deletes a claim, a failed claim write is `Failed`;
  manual `--resident` inside the window does not run; Admin entry skipped at sign-in and its dependents blocked;
  unknown elevation refuses; a real async launch seam (blocking launcher, 120 s timeout → Unconfirmed, retry
  refused, late completion releases and updates the row, cancel gate); `.bat` / `.cmd` / ambiguous unquoted
  paths; Startup-folder read failure; BOM and bounded reads; `install.ps1` uninstall order and exe selection; UI
  job tokens (a completion of another job never closes an editor).
* Rig GUI (fixtures `HayateStartupFixture-*` only, own slot, isolated `HAYATE_DATA_DIR`, foreground PID checked,
  normal user, no UAC, no real sign-in): see the Check 13 section of `docs/defender.md` for the run record and
  its limits. Cleanup was proven by comparing HKCU `Run` and `StartupApproved\Run` names with the saved baseline.
* *Not* proven on rig: a real sign-in run (needs a logon), Admin launch (elevation), elevated-Hayate launch path,
  HKLM writes, power loss during a write. These are covered by unit tests of the decision logic only.
