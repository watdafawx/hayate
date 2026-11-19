# Taskbar access (Jump List + optional Nilesoft Shell)

Right-click Blink's taskbar icon: **Open Blink**, **Processes**, **Vitals**, **Restart as administrator**.
Official Windows Jump List (`ICustomDestinationList`) on AUMID `Blink.TaskManager`. Nothing is hooked or injected. Task Manager is not touched.

Code: `src/taskbar/logic.rs` (pure, unit-tested), `src/taskbar/coord.rs` (ordering of every list change, tested with a fake shell), `src/taskbar/mod.rs` (COM, Nilesoft export, admin route), routing in `src/win.rs`.

## Tasks and command lines

| Task | Command line | Result |
|---|---|---|
| Open Blink | `blink.exe --open` | show the window |
| Processes | `blink.exe --tab processes` | show, switch to Processes |
| Vitals | `blink.exe --tab vitals` | show, switch to Vitals |
| Restart as administrator | `blink.exe --restart-admin` | explicit UAC prompt, new elevated Blink |

- Every task link is `"<installed exe>" <args>`, exe path quoted, args built with `join_args`. Icon is the exe's own.
- `--tab` takes: vitals, processes, history, startup, services, rules, watch. An unknown name just opens the window.
- A running Blink (hidden or visible) gets `--open` / `--tab` through the existing single-instance message `BlinkShowWindow` (wp 0 open, wp 2 + tab id). That message is let through the integrity filter (an elevated Blink must hear an unelevated launcher), so any process can post it and nobody can tell who did: it can only **show the window or pick a tab**. It can never start UAC, restart or quit anything. `--restart-admin` has no wire form (a unit test pins this). Not running (pinned): the new process applies the intent after its window exists.
- These one-shot args are in the update-relaunch private lists: a self-update never replays them.

## Elevation

- Normal launch never auto-elevates. Manifest stays `asInvoker`.
- `--restart-admin` is handled by **the process the task started**, not by a message to the running Blink. It asks UAC itself, `ShellExecuteW("runas", blink.exe, relaunch args)`, nothing else; the relaunch args exclude `--restart-admin` so it cannot loop.
  - No Blink running: UAC for a fresh elevated Blink.
  - An unelevated Blink running: UAC for an elevated Blink that replaces it (`--wait-pid <pid>`). The running Blink is **not touched** until UAC is accepted. Then the elevated copy asks it to quit with the message `BlinkQuitForAdminRestart`. That message is not let through the integrity filter: only a process of equal or higher integrity can post it (one that could end Blink anyway), and an elevated Blink never acts on it. The elevated copy only sends it to an unelevated Blink whose pid is the `--wait-pid` it was started with.
  - The running Blink is elevated already, or its token cannot be read: no prompt, just show it.
  - Already elevated launcher: the task just shows the window.
- UAC **No**: Windows reports cancelled. Both routes log `taskbar: restart as administrator: UAC cancelled, nothing started, the running Blink is untouched` and do nothing more. Any other failure is logged.
- No token handoff, no scheduled task, no auto-elevating COM. A stranger posting messages gets no UAC prompt.

## Install and persistence

Decision: **installed copies only** (user `%LOCALAPPDATA%\Programs\Blink` or per-machine). A portable copy, or any `BLINK_MULTI` instance, never registers: it would leave a pinned entry pointing into a throwaway folder. Settings shows the switch greyed out there.

| What | Where |
|---|---|
| Jump List (owned by Windows) | `%APPDATA%\Microsoft\Windows\Recent\CustomDestinations\<hash>.customDestinations-ms` |
| Switch (default on) | `<data dir>\setup.ini`, key `jumplist` |
| What was written (a cache; fingerprint, time, or `removed = true`) | `<data dir>\jumplist.ini` |
| Nilesoft snippet (Save button) | `<data dir>\nilesoft\blink-taskbar.nss` |

`jumplist.ini` is only a cache so a start does not rewrite the shell's list every time. It never decides whether an owned list is removed (see below). Content: `fingerprint` (16 hex) and `at` after a write, `removed = true` after a confirmed delete, or just a comment. Written by Blink, safe to delete.

Debug builds only: with the disposable `BLINK_TEST_AUMID=Blink.TaskManager.Test…` identity the copy may register, write and remove from anywhere (portable, `BLINK_MULTI`) because that list is its own throwaway. The release build ignores the variable, so a release copy can never register or remove from a portable folder or a test instance.

### How changes are ordered (`src/taskbar/coord.rs`)

Start sync, Settings switch and uninstall all go through one coordinator. Never on the UI thread, except that uninstall waits at most 3 s (the in-app uninstall) for its removal.

- One shell operation at a time. A request only marks "reconcile again"; the worker reads the **latest** switch, cache and install location each round and loops until nothing is pending. Switch off during the first registration ends removed. Switch on during a delete ends registered. A burst of toggles ends at the last position.
- Each shell operation has a **commit gate** just before its only persistent call (`CommitList` / `DeleteList`). The gate is checked and set under the coordinator lock together with cancellation, so cancel and commit cannot both win.
- **Timeout invariant:** after 20 s (write) / 10 s (delete) the worker cancels the operation. One that has not passed the gate can never change the list: when its detached thread ever wakes, the gate refuses and it aborts, so a stale write cannot overwrite, and a stale delete cannot remove, anything newer. One that is already inside the shell's commit cannot be taken back; nothing else starts until it returns, and its return wakes the worker, which then applies the latest position. A request that arrives while an operation times out is still honoured. No thread is ever killed.
- **Uninstall** (`remove_for_good`) closes the gate for writes for the rest of the process, under the same lock, then deletes once (after any commit already in the shell has returned). Two failed deletes give up (logged); no write can ever reach the shell afterwards. The window close waits up to 3 s more (`drain`) for a removal still running. If the shell is hung for the whole remaining life of the process, the list can be left behind (a stale entry pointing at a deleted exe; nothing recreates it).
- **The cache never decides removal.** Switched off with no `jumplist.ini` (never written here, file lost, cache write failed) deletes the owned list once, then writes `removed = true`. A failed delete leaves the old cache, so the next start or switch retries.

### Rules

- Written 8 s after start. Rewritten only when the fingerprint (AUMID, exe path, titles, args) changes, or after 14 days. Not per sample, not per start.
- Switch off: list deleted. Install moved or updated to another path: fingerprint changes, list rewritten.
- **Removal needs the same standing as registration:** installed (user or per-machine) and not `BLINK_MULTI`. A portable release copy running `--remove-jumplist`, or Uninstall from a `BLINK_MULTI` test instance, removes nothing: it never owned the global `Blink.TaskManager` list, and deleting it would take away the installed copy's. (Uninstall from a `BLINK_MULTI` instance of the installed exe therefore leaves the list; run `install.ps1 -Uninstall` or the unelevated exe's Uninstall.)
- Uninstall (Settings, or `install.ps1 -Uninstall` via `blink.exe --remove-jumplist`, headless) deletes the list and `jumplist.ini`.
- **Elevated `install.ps1 -Uninstall`** never runs the per-user `%LOCALAPPDATA%\Programs\Blink\blink.exe` (any process of that user can replace it, and the elevated script would then run it as admin). Elevated: only the per-machine exe under Program Files is asked to remove its list. Not elevated: only the per-user exe. So an elevated uninstall of a per-user install leaves that install's Jump List behind (the script says so). Run `install.ps1 -Uninstall` once from a normal terminal first to clear it, or use Settings > Uninstall. The leftover is a stale taskbar entry pointing at a deleted exe, nothing more.
- User removals of items are respected where the API allows (Windows keeps its own "removed" list for Recent/Frequent; tasks are app-defined and re-added on rewrite).
- Windows policy that disables jump lists (`NoStartMenuMFUprogramsList`, "Show recently opened items") hides Recent/Frequent. Whether user tasks still show under that policy was **not verified**; do not rely on it.
- Pins and icons: only the **primary Blink shortcut / taskbar icon** (`Blink.lnk`, AUMID `Blink.TaskManager`) has the tasks, pinned or running. `Blink (admin).lnk` and `Blink (no admin).lnk` have their own AUMIDs (`Blink.TaskManager.Admin` / `.User`) and show **no** tasks. Settings says so.

## Nilesoft Shell (optional, manual)

Windows has no supported way to add an entry to the empty-taskbar menu. Nilesoft Shell can. Blink never edits `shell.nss`, never restarts Explorer, never installs anything.

1. Settings > Taskbar > **Copy snippet** (clipboard) or **Save snippet file** (`<data dir>\nilesoft\blink-taskbar.nss`).
2. Saved file: add one line at the top level of your `shell.nss`: `import '<path shown in Blink>'`. Copied text: paste into your own `.nss` file and import that.
3. Nilesoft manager: Register, then restart Explorer.

Generated text (path example):

```
menu(type="taskbar" title="Blink" image=\uE9D9)
{
	item(title="Open Blink" cmd='"C:\Users\me\AppData\Local\Programs\Blink\blink.exe"' args='--open')
	item(title="Processes" cmd='"..."' args='--tab processes')
	item(title="Vitals" cmd='"..."' args='--tab vitals')
	item(title="Restart as administrator" cmd='"..."' args='--restart-admin')
}
```

- Both paths are checked before anything is written: the path of `blink.exe` (it goes into the items) **and** the path of the saved file (the user must put it in `import '...'`; it is inside the data dir, which `BLINK_DATA_DIR` can move). A `'`, `@`, `$`, `"`, `%` or control character in either is refused (Nilesoft would read them as syntax; `%` conservatively, as a possible variable). Nothing is created, the Settings note names the folder, the characters and the way out (a plainer install folder, a plainer `BLINK_DATA_DIR`, or **Copy snippet** and save it yourself). **Copy snippet** only needs the exe path. Portable copies refuse too.
- Blink never edits `shell.nss` and never restarts Explorer.
- Syntax sources: nilesoft.org docs (configuration, properties); `github.com/moudey/Shell` `src/bin/imports/taskbar.nss` and `shell.nss`; Nilesoft v1.9.18 on test machine, read-only.
- Not promised: Normal Windows users get no empty-taskbar entry without Nilesoft.

## Limits

- Elevated registration is not verified. Same-user elevated and unelevated copies share `%APPDATA%` and its per-user Jump List. Blink retains its existing data-dir trust (`BLINK_DATA_DIR`, `jumplist.ini`, the snippet); this feature adds no new arbitrary-content write: tasks are fixed, the cache holds a hash/time/flag, and the snippet is generated text.
- The shell-side races are tested against a fake shell with barriers (switch during registration and during delete, uninstall during prepare and during commit, timeouts, stale writers and deleters). The real list was exercised on test machine with a disposable debug AUMID; see the final report of the change for what was and was not clicked.
- `BLINK_TEST_JUMPLIST_DELAY_MS` (debug builds with the test AUMID only) slows every shell call before its gate, to rehearse those races against the real list.
- The real AUMID's list is only exercised on installed copies. Test machine tests use a debug-only disposable AUMID: `BLINK_TEST_AUMID=Blink.TaskManager.Test…` (debug builds only; the release build ignores it).
- Nilesoft snippet was compared with the shipped `taskbar.nss`, not run against a live Explorer.
