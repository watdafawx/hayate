# Disallow running

An opt-in saved rule: when a program matching the rule **has started**, Blink ends it. It is the Process Lasso "disallow" idea, built on Blink's existing rules engine.

## What it is not

- **Not launch prevention.** Windows still starts the program. It can show a window or run for a moment (measured on the test machine: about 1 s with the 1 s snapshot path, faster when ETW is available) before Blink ends it.
- **Not a security control.** It does not stop malware, does not survive Blink being closed, and a program that restarts itself faster than Blink ends it will win some rounds (see rate bound).
- **Unsaved work is lost.** `TerminateProcess` gives the program no chance to save. The panel says this first, before any other limit.
- **No new system footprint.** No driver, service, scheduled task, IFEO key, firewall rule, elevation request or autostart. It only runs while Blink runs, with Blink's own rights (so an elevated program, or one in another user's session, needs Blink run as administrator; otherwise the Rules tab shows "access denied").

The UI says this in the confirmation panel before anything is enabled.

## Rule model

`rules.ini`, one extra key on an ordinary `[rule]`:

```ini
[rule]
pattern = game.exe        ; exe name: every instance, any folder
; pattern = C:\Games\game.exe   ; or one exact file
enabled = true
disallow = true
```

- Default off. Existing rules are never migrated to block; a rule without `disallow` is unchanged. `disallow = true` with `enabled = false` is valid and ends nothing.
- **One switch per rule.** `enabled` covers the whole rule, so "Disallow saved but OFF" can only live in a rule that is itself OFF. A rule with `disallow = true` and `enabled = true` blocks.
- A **glob** pattern (`*`/`?`) can never disallow: the flag is dropped at load and on edit, with a visible warning. So is a pattern that only matches programs that are always protected (below), judged by name and path alone (no process or PID is involved): such a flag in a hand-edited `rules.ini` is dropped **at load** with a warning in the Rules tab (a rule left with nothing else is switched OFF, never shown as ON). A refusal when a rule is added or edited appears the same way (the newest 20 warnings are kept, no repeats).
- **Precedence** is the existing one: exact path beats name beats glob. The effective enabled rule decides. A more specific enabled rule *without* Disallow lets the program run (the user's explicit exception). When the effective rule blocks, no other action in that rule is applied (block wins); the other actions stay saved.
- **Clipboard import** (Paste rules) of a Disallow rule arrives OFF. "Paste onto" never copies or removes Disallow.
- Nothing is applied by opening a panel. Enabling goes through the panel (explicit text), never silently from a toggle.

### Saving over an existing rule (what the panel guarantees)

`Engine::set_disallow(pattern, mode)`:

| Existing rule for the same pattern | `Save, keep OFF` | `Disallow` (confirm) |
|---|---|---|
| none | new rule, flag set, **OFF** | new rule, flag set, ON |
| OFF, no other settings | flag set, stays OFF | flag set, ON |
| OFF, with settings (priority, affinity…) | flag set, stays OFF, settings kept | needs the explicit **"Turn the whole rule on + Disallow"** button: the rule goes ON, its other settings stay saved but are not applied while Disallow is on |
| ON, with settings | **refused, nothing changed**: an enabled rule cannot hold a dormant flag, and it keeps applying as before | same explicit button (its settings stop being applied) |
| ON, already Disallow | refused ("it is ON") | no change |

An enabled priority rule is therefore never switched off by "Save, keep OFF". **Undo** (`Allow running again`) removes the flag and puts the rule's enabled switch back to what it was before this panel saved, so other settings are never quietly turned on; a Disallow-only rule is removed. Removing Disallow from the Rules right-click menu keeps the rule's current switch and says "its other settings apply again" when there are any.

## UI

- Process menu (Processes and every Vitals Top list, shared builder): `Disallow running… (every name.exe)`. Disabled, with the reason, for protected programs, a process with no start time, no image path or no representative.
- Panel: re-reads the process through a handle verified by PID + creation time, uses *its* image (not the row text), runs the protection checks, then offers scope (every copy of the name / only this file). It shows the other settings of an existing rule and what turning Disallow on does to them.
- **The count is a label, not a promise.** It reads "N copies named like X were seen running in the last list refresh. After you confirm, Blink tries to end the ones it is allowed to…". Skipped or surviving: protected programs, ones needing administrator rights, other sessions' programs, ones covered by a more specific rule, and copies that start or exit meanwhile. The button says `Disallow, try to end N seen now`.
- Buttons: `Save, keep OFF` (ends nothing), `Disallow …` / `Turn the whole rule on + Disallow`, `Allow running again` (undo), `Retry saving`. The text area scrolls (mouse wheel) and the buttons wrap into rows pinned under it, so every limit and every button is reachable in a narrow or short window.
- **Save status.** Success text appears only after the disk write is acknowledged (`SaveStatus`). A failed write says so and that the change lasts until Blink closes. A write the disk has not acknowledged within 8 s is reported as *not confirmed* (never as saved), waiting stops and the buttons become usable again, and `Retry saving` performs a **real write of the current rules** (also when the change is already on in memory). The Rules tab shows a red line while the last write failed.
- If the worker thread that ends programs could not start or has stopped, the panel says "Disallow cannot run", the confirm button is off, and every Disallow rule shows `ON but NOT ENFORCED` plus a red line in Rules. Nothing claims to be active while nothing is ended.
- Rules tab: red `Disallow running · ON · ends it after it starts` / `OFF · nothing is ended`; `Last result` shows the last attempt (ended, access denied, refused, paused). Toggling ON asks via the panel; toggling OFF is immediate. Right-click: `Allow running again (remove Disallow)`.

## Engine (src/rules/disallow.rs, protect.rs)

All termination runs on a dedicated `blink-disallow` worker, never on the UI or ETW thread. The queue is finite (256 jobs). Triggers: a process start event (`Job::Start{pid, create}`), a rule being enabled (`Sweep(rule)`), Blink startup (`Sweep(all)`), and the recoveries below.

Termination path (`enforce`), every step fail-closed:

1. `open_verified(pid, create, QUERY_LIMITED|SYNCHRONIZE)`: PID + creation time must match the event. No basename fallback, no stale event path.
2. Live image read from that handle (`image_of_handle`). Missing image or start time → nothing.
3. The engine's own precedence on the live image → effective *blocking* rule, else nothing.
4. `protect::check` (below).
5. Rate gate (below).
6. Second verified handle with `PROCESS_TERMINATE`. The final "is this rule still enabled and still the match?" check and the `TerminateProcess(handle, 1)` call run under **one hold of the rules lock**: a disable, delete or edit either lands before the check (nothing is ended) or waits until the terminate call has been made. Only the 1.5 s wait on the handle is outside the lock; outcome recorded (ended / still running / access denied).

### Recovery (nothing dropped is lost for good)

- **Cooldown.** When a rule trips the storm guard it is paused for 15 s and the guard arms a per-rule recovery deadline (pause end + 0.5 s). The worker waits with `recv_timeout` until the nearest deadline, and checks deadlines after **every** job, so a continuous stream of jobs cannot starve it. At the deadline it sweeps that rule's survivors through steps 1-6 again (verified handle, live image, precedence, protections, rate gate). If the sweep itself trips the rate gate again, the deadline is re-armed; a program that keeps restarting is therefore ended at a bounded rate (30 per 10 s, then 15 s pause) rather than ever being given up on.
- **Queue overflow.** A full queue drops the job but sets ONE coalesced mark (3 s after the first drop, repeated drops do not move it). When it is due the worker re-checks every running process through the normal rate path. A saturated queue followed by silence still reconciles.
- **Final answers are not retried.** A process that was refused (protected) or access-denied is remembered per rule (retained while that identity lives; every 30 s, query-only checks remove proven exits and keep unreadable identities) and recovery sweeps skip it, so an elevated survivor never causes a retry loop. Editing, re-enabling or deleting the rule clears this memory.
- **Cancel and prune.** Disabling, editing, clearing Disallow from, or deleting a rule drops its guard, its pause, its deadline and its memory (and the overflow mark when no Disallow rule is on). The guard map only holds ids of existing Disallow rules, so it cannot grow without bound.
- **Worker failure is visible.** If the worker thread cannot be created, or a later job finds it stopped, `Engine::worker_problem()` is set, Disallow cannot be turned on, and enabled rules show as not enforced (see UI). A panic inside one job is caught and logged; the worker continues.

### Protection (never overridable, no force option)

`protect::check` refuses, with a reason shown in Rules:

- PID ≤ 4, Blink itself (PID and image), any program named `blink*` (helper, updater, parked or multi-instance copies);
- anything under the Windows folder and the Defender / Security directories. The Windows folder is **`GetWindowsDirectoryW`**, the others are known-folder (`SHGetKnownFolderPath`) paths; no environment variable is read (a process can change those);
- an essential-name list (`csrss`, `lsass`, `svchost`, `explorer`, `dwm`, `winlogon`, `msmpeng`, …) wherever the file sits;
- **`IsProcessCritical`** on the verified handle: critical or "cannot tell" → refused;
- session 0 (services) and unreadable session → refused.

Every path is canonicalised before comparing: `/` or `\`, `\\?\`, `\\.\`, `\??\` and `\\?\UNC\` prefixes, doubled separators, `.` and `..` (never above the drive or share root), trailing dots and spaces, case, and 8.3 short names (`GetLongPathNameW`, when the file exists). Limit: a symbolic link or junction that points into a protected folder is judged by the path the process was started from, not by its target; the critical-process and session layers and the essential-name list still apply.

The static layer also rejects creating a rule whose pattern could never act (`pattern_refusal`, pure name/path judgement). Access denied is reported honestly (admin vs. protected process wording), never retried with elevation; no privilege is requested.

### Rate bound and log

Per rule: more than 30 attempts in 10 s pauses that rule for 15 s (the pause is shown in Rules; survivors are swept afterwards, see Recovery). One log line per rule per **30 s** (counts are kept in the rule's stats); one line per 30 s for queue overflow. An auto-restarting app therefore cannot busy-loop Blink or flood `blink.log`.

## Known limits

- Detection latency: up to ~1 s (snapshot path) or event-driven when ETW works. On the test machine ETW was unavailable ("session busy"), so the snapshot path is what was exercised.
- While a rule is paused (15 s) or after a dropped start event, a copy that started keeps running until the recovery sweep (pause end + 0.5 s, or 3 s after the first dropped job), a rule edit or a Blink restart. A copy that survives the sweep because it was already given a final answer (protected, access denied) is not retried.
- The panel's count is what the last process-list refresh showed; it is not a promise about what will be ended.
- Blink must be running; there is no pre-start gate.
- A race exists between a process starting and Blink ending it; by design nothing is blocked before it starts.
- Elevated / protected targets need Blink elevated and are still refused if critical or in the Windows folder.
- A write that never completes (hung disk) is reported as not confirmed after 8 s; the change works in memory until Blink closes.

## Tests

- `rules::model`: parse/serialize round trip, glob and bad-value handling, disallow-only rules.
- `rules::protect`: static layer table; canonical path forms (prefixes, `.`/`..`, trailing dots/spaces, UNC); pure pattern judgement independent of any PID; Windows folder from the OS; OS layer checked against Windows' own critical answer for every openable process on the machine.
- `rules::disallow` / `rules::tests` (real disposable children = a copy of `ping.exe` under a unique name): ends a matching child; a different exe, a disabled rule, a more specific allow rule, a stale or forged start event and protected names/paths are NOT ended; a restart storm is bounded; **a paused rule's survivors are ended by the worker with no new event**; **a continuous job stream cannot starve recovery**; **repeated overload re-arms the cooldown**; **disable/delete cancel the cooldown and prune guards**; **a full queue sets one coalesced recovery that runs through the rate path**; **worker failure is surfaced**; **denied/protected instances are not retried**; **a disable between the final check and `TerminateProcess` is impossible (coordinated race test)**; keep-OFF never changes an enabled rule; whole-rule confirmation; undo restores the switch; failed write retried by a real write; protected/glob flags dropped at load and on edit with visible warnings; imported rules OFF; enabling sweeps; disabling stops enforcement.
- `ui::disallow`: the count text is a label not a promise; limits text names every limit and starts with unsaved work; an unacknowledged write times out; whole-rule confirmation logic; unenforced rules never shown ON.
- `ui::procmenu`: scope text and fail-closed item.

Test-machine proof (isolated slot, scratch `BLINK_MULTI` + `BLINK_DATA_DIR`, uniquely named fixture, foreground PID checked before each click).

Review corrections: Rules-opened confirmation preserves the pre-enable switch for Undo. Live refusal identities are never evicted due to cache capacity. Recovery uses a monotonic clock. Rules writes serialize off the UI thread and superseded queued writes cannot overwrite a newer acknowledged save. Access to other sessions depends on Windows token/ACL permissions; Blink requests no added privilege.
