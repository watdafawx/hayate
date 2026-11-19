# Replacing Windows Task Manager with Blink: mechanisms, guarantees, design

Research note, 2026-11-17. Branch `t3/task-manager-replacement-research`.

**Scope.** What it would take for an opt-in Blink to open instead of Task Manager on the standard
launch routes, with native Task Manager as the fallback when Blink is broken. Research only:
nothing was enabled, no registry value was written, no service/task/driver/hook was installed, no
GUI was launched on dev PC. Code signing was declined earlier and is not proposed again.

**Evidence labelling.** Each claim below is tagged:

- **[DOC]** first-party Microsoft documentation or Microsoft-authored article.
- **[SRC]** first-party source code of an established tool (System Informer).
- **[MEASURED]** read from this machine (Windows 11 26100.9818) without changing it.
- **[INFERRED]** follows from the above but was not executed or documented as such.
- **[UNVERIFIED]** community report or untested assumption.

---

## Bottom line

1. **IFEO `Debugger` is the broad image-launch interception mechanism identified.**
   It applies to launches of `taskmgr.exe`; coverage of each current Windows entry point
   remains to be tested. This is not a supported Task Manager replacement API.
2. **IFEO is not a supported "replace Task Manager" API.** It is the debugger-attach feature
   ([DOC] GFlags, "Debugging a Service"), used far outside its purpose. Microsoft ships the same
   trick in Process Explorer, so it is de facto tolerated, but there is no contract and no
   guarantee it survives servicing.
3. **It is machine-wide, admin-only state.** The key is `HKLM`, Users have read-only access
   ([MEASURED]); Process Explorer's own help calls it "a global setting that affects all users
   regardless of how they start Task Manager" ([DOC] PE help).
4. **A crash-safe fallback is achievable, but not unconditional.** It requires a *separate, tiny,
   protected launcher* that never depends on Blink being healthy. The launcher is then the single
   point of failure: if it is missing, quarantined or corrupt, Task Manager is broken for every
   user. That cannot be engineered away with this mechanism.
5. **Blink's current per-user install cannot be the IFEO target.** `%LOCALAPPDATA%\Programs\Blink`
   is user-writable, and the substitute for an auto-elevated `taskmgr.exe` runs with the elevated
   token ([INFERRED], see §2.5). That is a medium→high integrity escalation primitive.
6. **Recommendation:** keep native Task Manager intact for now and ship the explicit Blink launcher
   (tray, Jump List, hotkey, Start shortcut). If the replacement is still wanted, it must be a
   machine-wide launcher in `%ProgramFiles%`, enabled by UAC, with an enable-time probe, a
   Shift-at-launch escape hatch and a native fallback that never edits the registry. §7 is that
   design; §10 lists what is still unproven and §11 the decisions needed before any of it is built.

---

## 1. What the standard routes actually start

| Route | What Windows starts | Evidence | Confidence |
|---|---|---|---|
| Ctrl+Shift+Esc | `taskmgr.exe`, launched by the Windows shell | [DOC] shortcut list: "Ctrl+Shift+Esc — Open Task Manager"; [SRC] System Informer explicitly handles this route: "Ctrl+Shift+Esc starts Task Manager, which starts us when we're the replacement, with Shift still held" (`SystemInformer/main.c:1257-1260`) | high |
| Win+X → Task Manager | `taskmgr.exe` (Quick Link menu item) | [DOC] Win+X = "Open the Quick Link menu"; the menu item's target is not documented. [UNVERIFIED] that the item is a plain `taskmgr.exe` launch | medium |
| Taskbar right-click → Task Manager | expected `taskmgr.exe` | The user's Windows 11 screenshot includes this entry. Its actual launch target and interception coverage were not measured. | unverified target |
| Start menu / search "Task Manager" | pinned `Taskmgr.exe` shortcut | [DOC] Process Explorer help: the setting applies "regardless of how they start Task Manager"; IFEO matches by image file name, so the shortcut path is irrelevant | high |
| `taskmgr` / `taskmgr.exe` in Run, cmd, or another app | `taskmgr.exe` | [DOC] `ShellExecuteEx` resolves the file (System32) and calls `CreateProcess`; IFEO is keyed on the image name | high |
| Ctrl+Alt+Del → Task Manager | `taskmgr.exe` on the secure desktop | [UNVERIFIED] Process Explorer users report it (2009 article, Malwarebytes 2015); the PE help text only names Ctrl+Shift+Esc and the taskbar. Nobody documents the Winlogon path | low |
| A third party launching `taskmgr.exe` | `taskmgr.exe` | same as Run | high |

Two consequences:

- Coverage is a property of the *image name*, not of the shortcut or the menu. Anything that
  eventually calls `CreateProcess`/`ShellExecute` on `taskmgr.exe` is covered; anything that
  holds its own UI (a third-party "Task Manager" button that opens its own window) is not.
- `taskmgr.exe` is a WRP-protected system file: owner `NT SERVICE\TrustedInstaller`, only
  TrustedInstaller has full control, `SfcIsFileProtected` returns **true** ([MEASURED]; [DOC]
  "Windows Resource Protection"). Replacing, renaming or patching it is not an option: only
  Windows servicing may change it ([DOC] Supported Resource Replacement Mechanisms).

The Group Policy "Remove Task Manager" (`DisableTaskMgr`) only *blocks* Task Manager; it does not
redirect it ([DOC] ADMX_CtrlAltDel). It is not a replacement mechanism.

---

## 2. IFEO: what the mechanism really does

### 2.1 The key and the value

```
HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe
    Debugger = REG_SZ = "<full path to the replacement>"
```

[DOC] Microsoft's "Debugging a Service" documents exactly this key/value for launching a program
under a debugger; GFlags documents the same feature ("Specify that a particular program always
runs in a debugger"). Historically the value is a quoted full path and may carry debugger
arguments (`"c:\dbg\ntsd.exe -g"`), because the whole string is prepended to the command line
[DOC] (MSDN blog, Raymond Chen).

**ACL, measured on this machine** ([MEASURED]):

```
IFEO key:  BUILTIN\Users        ReadKey            (allow)
           BUILTIN\Administrators FullControl      (allow)
           NT AUTHORITY\SYSTEM  FullControl       (allow)
```

So enabling, repairing and removing it all need an elevated token. A standard user cannot even
undo it.

**Bitness.** The key that is consulted depends on the bitness of the *calling* process
([DOC] Gregg Miskelly, quoted by the MSDN blog), not of the target. The standard routes are
64-bit shell/AppInfo processes, so the 64-bit view is the one that matters; a 32-bit caller
launching `taskmgr.exe` directly would read `Wow6432Node`. Blink should write only the 64-bit key
unless a 32-bit route is proven to matter.

### 2.2 What the kernel does (and does not do)

[DOC] Raymond Chen, "Image File Execution Options just inserts the debugger in front of the
command line":

> the debugger is prepended to the command line and then CreateProcess resumes as if that were the
> command line you had passed originally. In particular, it doesn't do anything with the other
> parameters to the CreateProcess function... And the PROCESS_INFO that is returned by the
> CreateProcess function describes the debugger, not the process being debugged.

This matters more than anything else in this note:

- The replacement is a **normal process**, started with the target path (and its arguments) after
  the debugger string. It is **not** attached to anything and **no `taskmgr.exe` process is
  created**.
- There is therefore no debuggee whose death follows the replacement's death. The
  `DebugSetProcessKillOnExit` semantics ([DOC]) only apply if the launcher itself creates the
  target with `DEBUG_PROCESS`, which it does only in the fallback path.
- If the `Debugger` path is wrong, the user gets a file-not-found error instead of Task Manager;
  ATT&CK describes the same effect as "redirect and effectively disable" ([DOC] MITRE T1546.012).
  This is the known failure mode of Process Explorer's option.

### 2.3 Recursion and evidence for a bypass

[DOC] MSDN blog, "How does IFEO work?":

> Kernel32!CreateProcess when called without the DEBUG_PROCESS or DEBUG_ONLY_THIS_PROCESS creation
> flags, checks the registry... Now ntsd eventually launches notepad under the debugger by calling
> Kernel32!CreateProcess with one of the following creation flags... The presence of any of these
> creation flags forces Kernel32!CreateProcess to bypass IFEO options this time around (else we
> would have been running into an endless loop).

So a native Task Manager launched **with `DEBUG_PROCESS`** is not redirected, with no registry
change at all. This is the whole "native fallback without racy key toggling" answer. It is also
what System Informer ships: `PhCreateProcessIgnoreIfeoDebugger` clears the PEB
`ReadImageFileExecOptions` flag, creates the process with
`PH_CREATE_PROCESS_DEBUG | PH_CREATE_PROCESS_DEBUG_ONLY_THIS_PROCESS`, disables kill-on-close on
the debug object and removes the debug object before returning
(`SystemInformer/appsup.c:1783-1855`, commit `cb1af3a5858cbb20bf1e0786c882c8024fd52fe8`).

The PEB flag manipulation is an internal implementation technique, not a public
Windows compatibility contract. The documented debug creation flags and an older
Microsoft-authored blog do not make the entire sequence a supported replacement
or guarantee current Windows behavior. Treat it as a prototype requiring validation.

Why it works without SeDebugPrivilege in principle: the creator gets full access to its own child,
so detaching does not need debug privilege for another process. System Informer still routes this
through its elevated service when it is itself unelevated on Windows 8+ (`mainwnd.c:1650-1665`);
the source does not explain why, and it is not documented. That is one of the items to test (§10).

### 2.4 The argument the replacement receives

The debugger string is prepended, the original command line follows. For Task Manager the
replacement sees something like:

```
"C:\Program Files\Blink\blink-tm.exe" C:\Windows\System32\taskmgr.exe
```

The target may arrive as a bare name or a full path depending on the caller, and extra shell
arguments are possible. The launcher must therefore **not** trust `argv[1]`; it should resolve
`%SystemRoot%\System32\taskmgr.exe` itself for the fallback.

Also from the same [DOC]: `STARTUPINFO` is passed to the debugger, so `SW_HIDE` would hide *the
replacement*, not Task Manager. Blink must show itself explicitly rather than rely on the
inherited show state.

### 2.5 Elevation: the part nobody documents

[MEASURED] `taskmgr.exe` manifest (resource 24, read from the file):

```xml
<requestedExecutionLevel level="asInvoker"/>
<asmv3:windowsSettings><autoElevate>true</autoElevate></asmv3:windowsSettings>
```

[DOC] Microsoft's "Inside Windows 7 User Account Control" states the same and shows the sigcheck
dump: "Task Manager is opted in for auto-elevation". Auto-elevation is performed by the shell /
Application Information service, which inspects `taskmgr.exe`'s manifest and then creates the
process with an **elevated token** ([DOC] UAC architecture: `ShellExecute` → AppInfo →
`CreateProcess`).

The IFEO substitution happens *inside that `CreateProcess`* ([DOC] Chen: the substitution is done
by `CreateProcess` itself). Therefore the substitute is very likely created with the same elevated
token, i.e. **Ctrl+Shift+Esc would silently start the replacement elevated, with no UAC prompt**
[INFERRED, not executed]. Note the fork: this only holds if the route reaches `CreateProcess`
through the auto-elevating shell path (`ShellExecute`/AppInfo). If a route calls `CreateProcess`
directly, `taskmgr.exe` is created with the caller's token (`asInvoker`) and the substitute is
unelevated. The probe in §7.2 distinguishes the two. Consequences:

- The replacement must be trustworthy at high integrity: a user-writable target becomes a UAC
  bypass (medium-IL code writes the file; the next elevated Task Manager launch executes it).
- The replacement, if it is Blink, would run elevated on every Task Manager keypress, reading
  Blink's user-writable configuration (`rules.ini`, `setup.ini`, `history`). That is a
  config-file escalation path that must be handled if Blink is ever the elevated process.
- The fallback path also inherits the elevation: a Task Manager created by an elevated launcher is
  elevated, which matches the native experience. An unelevated launcher would produce an
  unelevated Task Manager (still functional, but a downgrade).

This is the single most important thing to verify on test machine before any design is approved (§10).

---

## 3. What the established tools actually do

### 3.1 Process Explorer (Sysinternals, closed source)

[DOC] Process Explorer help, "Replace Task Manager":

> Select the Replace Task Manager entry under the Options menu to have Process Explorer execute
> instead of Task Manager when you launch Task Manager. Note that this is a global setting that
> affects all users regardless of how they start Task Manager. After replacing Task Manager the
> menu item renames to Restore Task Manager and selecting it removes Process Explorer's
> association.

(Help text quoted from the shipped `procexp.chm`; mirrored at
<https://documentation.help/procexp/documentation.pdf>.)

Observable behaviour, corroborated by Microsoft Q&A and third-party write-ups: it writes the
`Debugger` value under `IFEO\taskmgr.exe`, requires elevation, and "Restore Task Manager" deletes
it. Weaknesses, all of which Blink must not copy:

- **No saved previous value.** Restoring deletes whatever was there.
- **No fallback.** If `procexp.exe` is moved or deleted, Task Manager is broken
  ([UNVERIFIED] widely reported; consistent with [DOC] Chen). Users are told to edit the registry
  to recover.
- **No escape hatch inside the mechanism.** Recovery requires a working Process Explorer or
  regedit.
- The setting is reported to be reset by some feature updates
  ([UNVERIFIED] Microsoft Q&A thread 4042289; the moderator says "might undo the setting, I'm not
  sure"). No Microsoft documentation promises persistence.

### 3.2 System Informer (open source, formerly Process Hacker)

This is the clearest first-party evidence of how the mechanism is implemented in a shipping tool
(all citations to commit `cb1af3a5858cbb20bf1e0786c882c8024fd52fe8`):

| Behaviour | Evidence |
|---|---|
| Writes `HKLM\...\Image File Execution Options\taskmgr.exe` `Debugger` | `SystemInformer/options.c:162-163`, `1104-1200` |
| Value is the quoted full path of its own exe | `options.c:1177-1195` (`PhConcatStringRef3` with `"` separators) |
| Enabling needs elevation (button shows a UAC shield when not elevated) | `options.c:1106-1122`, `1524-1530` |
| "Restore default" deletes the value, without preserving a previous one | `options.c:1172-1177` |
| Warns (only) when not under `%ProgramFiles%\` | `options.c:1750-1800` (`PhShowOptionsDefaultInstallLocation`, string-prefix check, no ACL check) |
| Recognises "we are the default" by comparing the value with its own path | `options.c:1092-1122` (`PathMatchesPh`, allows a quoted value) |
| Escape hatch: **hold Shift (without Ctrl) at launch** → start real Task Manager instead | `main.c:1247-1280`, called at startup (`main.c:72`) |
| Explicitly handles the Ctrl+Shift+Esc case: the shortcut arrives with Shift still held, so Ctrl must be *up* to count as a deliberate Shift start | `main.c:1254-1260` |
| "Start Task Manager" tool launches native TM with IFEO bypassed | `mainwnd.c:1624-1666` via `PhCreateProcessIgnoreIfeoDebugger` (`appsup.c:1783-1855`) |
| Unelevated on Windows 8+ → routes the bypass through its own elevated service | `mainwnd.c:1650-1665`; `phsvc/svcapi.c:1380-1400` |
| No fallback if System Informer itself is broken: Shift is read by System Informer, so a broken/absent binary cannot be bypassed at launch | by construction |

Lessons:

- Shift-as-escape is a good idea but **must live in the launcher, not in Blink**, and it must
  ignore the Ctrl+Shift+Esc case. System Informer got that detail right; Process Explorer has no
  escape hatch at all.
- The "secure location" check in the established tool is a string prefix, not an ACL/ownership
  check. Blink would need the stronger check.
- Neither tool guarantees fallback. Both can leave the machine with no Task Manager.

---

## 4. Options that do not work, and why

| Option | Why not |
|---|---|
| Replace/rename `taskmgr.exe` | WRP-protected, TrustedInstaller-owned, `SfcIsFileProtected`=true ([MEASURED], [DOC]). Only servicing may replace it. |
| Copy `taskmgr.exe` to another name and run that | Not supported. The image is WRP-managed and its WinUI content/`TaskManagerApp.dll` expectations are tied to its location/identity; users report a blank window and odd integrity when renamed ([UNVERIFIED] Super User 661017). A copy in a user directory is also a classic binary-proxy/EDR signal. |
| `HKCU\...\App Paths\taskmgr.exe` | Search order is documented as CWD → Windows → **System32** → PATH → App Paths ([DOC] Application Registration), so it cannot shadow the real file. It also cannot affect Ctrl+Shift+Esc / Win+X, which use full paths. |
| `RegisterHotKey(Ctrl+Shift+Esc)` | No documented extension point; the docs say registration typically fails when the combination is already taken, and OS-reserved shortcuts are special-cased ([DOC] RegisterHotKey). Even if it worked it changes one route, not Task Manager. |
| Shell/taskbar hooks to swap the menu item | No documented extension point. Nilesoft Shell is third-party and manual (already shipped as an optional snippet, `docs/taskbar.md`); it adds entries, it does not replace Task Manager. |
| `DisableTaskMgr` policy | Blocks Task Manager, per user ([DOC] ADMX_CtrlAltDel). Not a redirect. |
| Application Compatibility shim (`RedirectEXE`) | General shim infrastructure exists, but no primary source endorses redirecting `taskmgr.exe`; machine-wide, admin, same user-writable-target risk, plus a servicing/deprecation surface. Not evaluated further; not recommended. |
| IFEO with `cmd.exe /c ...` or a script as `Debugger` | The child is created with `DEBUG_PROCESS` only by the fallback; a shell wrapper cannot host the bypass, adds a console/window, and multiplies failure modes. Rejected. |
| "Temporarily delete the value, launch Task Manager, restore it" | Racy (any concurrent request goes native), needs admin each time, leaves the replacement off if the restore fails, and is exactly the noisy pattern this note exists to avoid. Rejected in favour of the `DEBUG_PROCESS` bypass ([DOC]). |

---

## 5. Trust boundary and threat model

### 5.1 The value is a machine-wide code-execution pointer

`HKLM\...\IFEO\taskmgr.exe\Debugger` runs a program, as the requesting user, every time Task
Manager is launched. Combined with `autoElevate` (§2.5):

- **Same-user escalation**: an admin user's medium-IL malware overwrites the target exe; the next
  Ctrl+Shift+Esc runs it at high integrity. This is a UAC bypass built from a documented feature.
- **Cross-user escalation**: if the target lives in a user-writable profile and another admin
  launches Task Manager, that admin executes the first user's bytes elevated.
- **Attack surface for EDR**: the write itself is the canonical IFEO-injection signal
  (MITRE T1546.012; Defender/EDR behaviour rules). Blink will be flagged for doing what it does,
  regardless of intent.

Therefore, invariants:

1. The `Debugger` path must be in an admin-only-writable directory. `%ProgramFiles%` is
   read/execute for Users ([MEASURED]); `%LOCALAPPDATA%`, `%APPDATA%`, `%TEMP%`, `%ProgramData%`
   root are not acceptable.
2. At enable time and at every start, verify the whole chain: directory owner/ACL (no
   non-admin write ACE on the file or any parent up to the volume root), no reparse point in the
   path, file exists, expected version/name. A string prefix check (what System Informer does) is
   not enough.
3. Never execute a user-writable binary from the elevated launcher. The proposed design only
   *messages* a running Blink and launches `%SystemRoot%\System32\taskmgr.exe`; it starts no
   user-writable code.
4. If Blink were ever cold-started by an elevated launcher, it would run elevated from a
   user-writable install and read user-writable config — an escalation and a config-injection
   surface. Do not do that (see §7.3 for the resident-only rule).
5. Never overwrite or delete a `Debugger` value that is not ours (another debugger/tool), and
   always save/restore the previous value.
6. Code signing is declined, so ACLs are the only integrity boundary. That is acceptable for a
   machine-wide Program Files binary but must be checked, not assumed; and the app must expect
   Defender/EDR to dislike the IFEO write.

---

## 6. What "crash-safe fallback" can and cannot mean

### 6.1 Guaranteed by construction (with the §7 design)

- Blink not running → native Task Manager.
- Blink hung → the launcher's cross-process send times out → native Task Manager.
- Blink crashed/panicking on the previous request → it is not running → native Task Manager.
- Blink updated/replaced → the launcher is a different file and does not care.
- Blink single-instance/helper exit codes → irrelevant: the launcher does not start Blink, so
  there is no "helper exited 0 because another instance handled it" taxonomy to get wrong.
- The fallback never touches the registry, so there is no window in which concurrent requests go
  native and no restore step that can fail.

### 6.2 Not guaranteeable

- **The launcher itself.** If `blink-tm.exe` is deleted, quarantined by AV, corrupted, or the
  `Debugger` value points at a stale path, the Task Manager routes are dead for every user. The
  `DEBUG_PROCESS` bypass cannot run because nothing runs. This is inherent to IFEO.
- **AppInfo/auto-elevation behaviour.** If a future Windows stops auto-elevating `taskmgr.exe`, or
  changes how the substitution is tokenised, the replacement may launch at a different integrity
  level or not at all. Fail-safe (native TM) only if the launcher is still there.
- **Servicing.** A feature update may remove the value, or a Task Manager redesign may stop using
  `taskmgr.exe`. Detect and report; never silently re-add.
- **A machine with no administrator.** Enable/repair/remove all need UAC with admin credentials.
- **The "no Task Manager at all" window** between an AV quarantine of the launcher and the user
  noticing. Only monitoring + a documented manual `reg delete` can shorten it.

### 6.3 Why the fallback must use `DEBUG_PROCESS`, not key toggling

Covered in §2.3. Additional properties of the bypass:

- It creates the fallback Task Manager with the launcher's token. Elevated launcher → elevated
  Task Manager (native experience). Unelevated launcher → unelevated Task Manager (works, but
  can't act on other users' processes).
- It must set `DebugSetProcessKillOnExit(FALSE)` and detach (`DebugActiveProcessStop`, documented:
  "To stop debugging the process, you must exit the process or call DebugActiveProcessStop";
  exiting the debugger otherwise kills the debuggee). System Informer does the equivalent with
  `NtRemoveProcessDebug`.
- It clears the PEB `ReadImageFileExecOptions` flag as System Informer does, so other IFEO
  settings (GlobalFlag/page heap) are not applied to the fallback either. That flag is
  undocumented; the bypass itself only needs `DEBUG_PROCESS` ([DOC] blog).
- No registry mutation, no global window, no race, no recursion.

---

## 7. Proposed minimal design (not built)

### 7.1 Components

| Component | Location / identity | Why |
|---|---|---|
| `blink-tm.exe` launcher | `%ProgramFiles%\Blink\blink-tm.exe`, admin-only ACL, stable path, ~50 KB, no GUI, no COM, no network, no config parsing | The IFEO target. Must be tiny and boring because it is the single point of failure. |
| Machine state | `HKLM\SOFTWARE\Blink\TaskManager`: `Enabled`, `LauncherPath`, `BlinkPath`, `PreviousDebuggerExisted`, `PreviousDebugger`, `SchemaVersion` | Admin-owned; the launcher reads only `BlinkPath` from here, never from user-writable input. |
| Blink (existing, per-user) | `%LOCALAPPDATA%\Programs\Blink\blink.exe`, tray resident | Shows the UI. Elevated only if the user asked for it. |
| Enable/disable helper | `blink.exe --taskmgr-enable` / `--taskmgr-disable` / `--taskmgr-repair` / `--taskmgr-probe`, launched with `runas` | All registry writes happen in one elevated, auditable place. |
| Settings/tray surface | "Use Blink for Task Manager" + state + Restore + Test | Consent and recovery without regedit. |

No service, scheduled task, driver, hook or startup entry is added by this feature. The launcher
is not persistent: it runs only when Task Manager is launched.

The Jump List / Nilesoft taskbar work (`docs/taskbar.md`) is a separate, already-shipped feature and
is not part of this mechanism; it adds entry points to Blink and never touches Task Manager.

### 7.2 Enable flow (one UAC prompt)

1. Unelevated Blink: confirm dialog naming the exact key/value and that it is machine-wide.
2. Elevated helper:
   - verify the launcher exists, is not a reparse point, and the ACL chain up to the volume root
     grants write only to Administrators/SYSTEM/TrustedInstaller ([§5](#5-trust-boundary-and-threat-model));
   - verify the current `Debugger` value is absent or ours; if another tool's, stop and report;
   - save the previous value (or "did not exist") in `HKLM\SOFTWARE\Blink\TaskManager`;
   - write `Debugger = "<launcher path>"`;
   - **probe** (below);
   - on probe failure: restore the previous value (or delete), report the exact failing step,
     leave Task Manager native.
3. Write `Enabled=1` only after a successful probe. Show "enabled (all users)" in Settings.

**Probe (headless, no windows).** The elevated helper calls `CreateProcess` on
`%SystemRoot%\System32\taskmgr.exe` with a marker argument (e.g. `--blink-self-test <nonce>`).
IFEO prepends the launcher, which sees the marker and performs the real decision path in dry-run
mode:

- confirms it is running elevated;
- confirms it can find and hand-shake a running Blink (or records "not running" — that is a valid
  state, not a failure);
- exercises the native fallback: create `taskmgr.exe` with `DEBUG_PROCESS` and terminate it before it
  is ever continued (no window is shown), proving the creation and detach sequence works;
- exits with a result code the helper maps to a message.

This turns the machine-specific assumptions (the launcher's token integrity, that IFEO is reached
on this build, that the fallback sequence works) into observed facts and fails closed. It cannot
prove the Win+X / Ctrl+Alt+Del routes, servicing behaviour or EDR reaction; those stay in §10.

### 7.3 Launch flow (every Task Manager request)

> **Superseded in part (2026-11-18, branch `t3/launcher-cold-start-trusted-binary`).** The launcher now
> cold-starts Blink when none is running, from its own protected image only: see §7.3a. The
> "resident-only" statements below describe the original design; §7.3a and §14 "Cold start" win.

**Important scope limit:** this resident-only design exits after successfully
showing Blink. It falls back when Blink is already absent or unresponsive at the
request; it does **not** automatically open native Task Manager if Blink crashes
later. That additional user requirement needs a separate per-session watcher
holding the verified Blink process handle, a readiness handshake and explicit
normal-exit/update/uninstall signals to avoid false fallback. No watcher design
has been implemented or tested here.

```
IFEO → blink-tm.exe  (elevated, Program Files)
  1. Shift held without Ctrl?            → native Task Manager, exit
  2. FindWindow("BlinkMain") in my session
  3. verify that window's process image equals the `BlinkPath` recorded at enable
     time, or the standard per-user install path (message only; never executed)
  4. found:
       AllowSetForegroundWindow(that pid)
       SendMessageTimeout(hwnd, registered "BlinkTaskManager", 2000 ms)
         returned + window visible → exit 0
         timeout / error           → native Task Manager
     not found                     → native Task Manager
  5. native Task Manager:
       clear this process's PEB ReadImageFileExecOptions flag
       CreateProcess(%SystemRoot%\System32\taskmgr.exe,
                     DEBUG_PROCESS | DEBUG_ONLY_THIS_PROCESS)
       DebugSetProcessKillOnExit(FALSE); detach; exit
       on failure: exit with an error code, no further fallback exists
```

Key properties:

- **Resident-only.** The launcher never starts Blink, so it never executes user-writable bytes
  elevated and never has to classify a child's exit code. If Blink is not resident, the user gets
  native Task Manager (and a tray hint if Blink is installed but not running).
- **Readiness is tested, not assumed.** `SendMessageTimeout` is a direct test of Blink's UI thread.
  A hung Blink is detected in ≤2 s without process-level crash detection.
- **Single instance is Blink's own problem**, already implemented (registered `BlinkShowWindow`,
  `src/win.rs:89-133`; a second process forwards and returns). The launcher adds no process.
- **Concurrency**: several simultaneous requests all message the same window; the second one just
  re-shows it. Several native fallbacks collapse into Task Manager's own single instance.
- **No restart loops**: the launcher starts nothing that could crash and be retried. The only
  repetition is the user pressing the key again. (If optional cold start is ever added, this
  property is lost: a Blink that crashes on start would be retried on every Task Manager request
  and needs a circuit breaker.)
- **Foreground**: `AllowSetForegroundWindow` from the launcher (the process the shell started for
  the user's keypress) lets Blink's `SetForegroundWindow` succeed; otherwise the window may only
  flash its taskbar button.
- **Elevation**: the launcher is elevated (as the substitute for an auto-elevated image). It
  writes nothing, reads only HKLM and Program Files, and starts only System32 binaries.
- Optional later (cold-start variant): start Blink through `IShellDispatch2::ShellExecute` (the
  documented de-elevation already researched in the Defender review notes §4a) when Blink is not running,
  with the same 2 s budget and native TM on failure. Not part of the minimal design, because
  Explorer COM fails exactly when Explorer is unhealthy.

### 7.3a Cold start from the protected image (implemented)

```
IFEO -> blink-tm.exe (elevated, Program Files)
  1. Shift without Ctrl -> native Task Manager (unchanged)
  2. launcher file + every folder above it admin-only (as before)? no -> native
  3. Blink window (recorded BlinkPath image OR blink-tm.exe image) answers in 2 s -> shown, exit
     window exists but does not answer (hung)                                     -> native
  4. none: take Global\BlinkTaskmgrColdStart (<= 3 s), then
       open blink-tm.exe with share = READ only (no write/delete/rename while we check),
       re-run the ACL/link chain on it, rebuild the environment, CreateProcess(blink-tm.exe)
       command line = the quoted path only, cwd = %ProgramFiles%\Blink
  5. poll for its window <= 3 s: appeared -> exit 0
       no window / the process already ended -> TerminateProcess(child), native Task Manager
  6. native failed too -> message box (as before)
```

**What is started.** Never a path from argv, the environment, the registry or the user's disk:
only `blink-tm.exe` itself, the file the launcher is already running from, after the same
check the enable step uses (owner SYSTEM/Administrators/TrustedInstaller, no non-admin write/
delete/DAC/owner ACE, no reparse point, path exactly `%ProgramFiles%\Blink\blink-tm.exe`).
`%ProgramFiles%\Blink\blink.exe` is not used: `install.ps1 -Machine` can put one there, but the
state records nothing that vouches for it, so one start path means one thing to verify.

**How the started copy knows to run as the app.** It is also named `blink-tm.exe`, so `launcher()`
would treat it as a launcher. It runs as the app only when (a) it is elevated, (b) its parent
process image is this same file and (c) the file chain is still admin-only (`cold_child`). argv
and the environment cannot say so (the caller of an elevated launcher writes both). A non-admin who
starts the protected exe by hand gets the launcher path without elevation. Faking the parent (parent-PID
spoofing) needs `PROCESS_CREATE_PROCESS` on the high-integrity launcher process, which a medium-integrity
caller cannot open, **and** `SeAssignPrimaryToken` + `SeIncreaseQuota` to hand the child a token it did not
inherit; an administrator has both and is outside the threat model (§5).

**Clean start.** The command line is the quoted path only. The environment is a **whitelist**
(`child_env` + `system_vars`, fix 2026-11-18 "launcher-cold-start-fixes"; the first version only
stripped `BLINK_*` and overrode eight variables, which left e.g. `ProgramFiles` caller-controlled):
`LOCALAPPDATA APPDATA USERPROFILE HOMEDRIVE HOMEPATH TEMP TMP SystemRoot windir SystemDrive ProgramFiles
ProgramW6432 ProgramFiles(x86) CommonProgramFiles ProgramData ALLUSERSPROFILE COMSPEC PATHEXT PATH` come from
the known-folder API and the Windows folder; from the caller only `NUMBER_OF_PROCESSORS`, `PROCESSOR_*`,
`OS`, `USERNAME`, `COMPUTERNAME`, `USERDOMAIN` pass (they name nothing to load or run). Every other
variable the caller set is dropped, `BLINK_*` and the recursion guard included. The current directory is
the protected folder. Why it matters: the medium user can set `HKCU\Environment\ProgramFiles` and plant
a DLL there; the elevated child used to `LoadLibraryW(%ProgramFiles%\PawnIO\PawnIOLib.dll)` (CPU temp
sensor) and would have loaded it as administrator. Second layer: code that builds a load/exec path no
longer reads the environment at all. `sensors.rs` uses `setup::program_files()`; `rules/actions.rs`
(explorer.exe) uses `osutil::windows_dir()` (`GetSystemWindowsDirectoryW`). Remaining `env::var`
reads of `SystemRoot`/`LOCALAPPDATA` are display / unelevated-only or tests (`ui/icons.rs` icon source,
`ui/procs.rs` grouping, `watch/*` path matching, `setup` per-user install dir) and load or run nothing.

**Data dir (elevated cold child).** The cold-started Blink is elevated but its data dir is the medium
user's `%LOCALAPPDATA%\Blink`. Chosen policy: (1) it writes **no `blink.log`** (`osutil::disable_log()`, the
same rule as the helper: `log_allowed(elevated)`); (2) `osutil::data_dir()` for an elevated process
refuses a dir that is, or sits below, a reparse point (junction / symlink / mount point): it returns an
unusable path (`|:\blink-data-dir-refused`) so reads see defaults and writes fail, instead of an elevated
writer following the link. Not done: a fully read-only mode. History, rules, startup, setup and the
other ini files are still read and (for settings the user changes) written in that folder, so the
residual risk is: the medium user can edit those files (they could anyway, they are theirs), and a
pre-planted **file-level** link or hard link inside the real dir is followed by an elevated write. A
read-only elevated mode would hide the user's settings and history in the window the user just asked
for; revisit if the cold-started Blink gets features that write more.

**Secure desktop.** Ctrl+Alt+Del and UAC run on the Winlogon desktop; the launcher can be started
there, `FindWindowExW` never sees the child on that desktop, and the old flow burned 3 s, terminated a
healthy elevated Blink and could leave an invisible one. The launcher now reads its thread's desktop name
(`GetThreadDesktop` + `GetUserObjectInformationW(UOI_NAME)`); anything but `Default` (or an unreadable
name) goes straight to the native Task Manager (`interactive_desktop`, `NativeWhy::SecureDesktop`),
before any window search, lock or start.

**Lock result.** The lock is an exclusive file handle (`CreateFileW`, share mode 0, `OPEN_ALWAYS`) on
`%ProgramFiles%\Blink\.coldstart.lock`. The folder is admin-write-only but **readable by Users**, and a
file created in it inherits that Users read/execute entry: with the default DACL a medium user could open
it read-only with share mode None and hold it, forcing the native Task Manager (found on the test machine; the
earlier "cannot be squatted" claim was wrong). So the file is created with its own protected DACL
(`LOCK_SDDL` = `D:PAI(A;;FA;;;SY)(A;;FA;;;BA)`: SYSTEM and Administrators only, nothing inherited): a medium
user cannot open it at all, not even read-only. An existing file is checked first
(`lock_file_policy`: owner and every allow entry must be SYSTEM / Administrators / TrustedInstaller); a
file with any other allow entry is deleted and created again, and if that delete fails (someone holds it)
there is no lock and the launcher goes native. The DACL of what we hold is checked once more after the
open. This also beats the earlier `Global\BlinkTaskmgrColdStart` mutex, which any user could create first.
`lock_try` separates the outcomes: `Got`; `Busy` (ERROR_SHARING_VIOLATION /
ERROR_LOCK_VIOLATION: another start holds it, retried every 50 ms for up to 3 s); `Unusable` (any other
error: no lock at all, no retry). Busy for the whole wait or unusable: the launcher looks once more for a
Blink window, shows it if there is one, and otherwise opens the native Task Manager (`NativeWhy::Busy`);
it never starts a Blink without the lock. The file is empty and harmless while the redirect is on; turning
the redirect off (and `install.ps1 -UninstallTaskManager` / `-Uninstall`) deletes it together with the launcher and
the then-empty `%ProgramFiles%\Blink` folder (best effort: a running launcher still holds its files).

**Desktop check is name-only (residual, self-inflicted).** `interactive_desktop` compares the desktop *name*
with `Default`. A medium user can start taskmgr on a private window station whose desktop is also named
`Default`: the launcher then cold-starts an elevated Blink nobody can see, and the 3 s window wait kills it.
Self-inflicted denial for that user only; no escalation (nothing of theirs runs elevated).

**Data dir check is check-then-use (residual).** `osutil::data_dir` tests for a reparse point and later
code opens files there; a medium user could swap a folder for a junction in between. Narrow, and the elevated
cold child writes no log; settings files in that folder are the user's own. Not closed.

**No hard-coded Windows folder; no winget in the cold child.** `osutil::windows_dir()` returns `Err` when
`GetSystemWindowsDirectoryW` fails (no `C:\Windows` guess for exec paths); callers skip the action. The cold-started
Blink (`osutil::cold_started()`) has a minimal PATH, so "Install PawnIO" (bare `winget.exe`) is hidden there with
the note "Run Blink normally to install PawnIO"; in a normal Blink a failed ShellExecute is now shown.

**Cross-integrity message channel.** The elevated Blink window calls
`ChangeWindowMessageFilterEx(hwnd, show_msg, MSGFLT_ALLOW)` so the launcher (when it runs unelevated) can
send the registered "show" message. That is a medium -> high channel: any medium-integrity process can send
that one registered message with wParam 3 ("Task Manager was asked for") and make the elevated window
come to the front. The handler does nothing else (no command, no path, no data is carried) and wp 3 is the
only value acted on; it is not new in the cold start, but now that the elevated Blink is started by the
launcher it is the one path from the user's session into the elevated window, so it is recorded here and
in the Defender review notes. Residual risk: a harmless focus steal.

**TOCTOU.** Check and use sit on one handle-protected file: the launcher opens `blink-tm.exe` with
`FILE_SHARE_READ` only, re-runs the chain, calls `CreateProcess` and only then closes the handle.
While it is open nobody can write, delete or rename the file (or its folder). What remains is an
administrator replacing a folder above Program Files, who already controls the launcher and the IFEO
value itself (outside the threat model of §5).

**Elevation.** The launcher is elevated (it substitutes an auto-elevated image) and so is the Blink
it starts: the same state as a Blink the user restarted as administrator (Jump List "Restart as
administrator"), with the same consequences: non-elevated automation cannot drive its window
(UIPI), the tray icon and window belong to the elevated process, and "run as normal user" actions
use the existing unelevated launch path. Its data dir is the elevated token's
`%LOCALAPPDATA%\Blink`, the user's normal folder when it is the same account (over-the-shoulder
elevation with another admin account uses that account's folder). No UAC prompt is added: the
launcher already runs elevated, silently, as in the resident-only design.

**Single instance.** The started copy goes through the existing `FindWindow("BlinkMain")` check in
`win::run`: with another Blink already there it forwards and exits, the launcher sees the child
end and falls back. Two near-simultaneous requests are serialized by the lock file, so the second
finds the first's window instead of starting another Blink.

**Update / install flow of the started copy.** `blink-tm.exe` in Program Files classifies as a
machine install: no install offer, self-update off (the update code needs `blink.exe` in the install
dir), no per-user shortcut or startup writes. Updating Blink does not refresh it by itself; Settings shows
"The Task Manager launcher is older than this Blink" with a [Refresh launcher] button (see §15), and an
elevated Blink refreshes it once at start. Turning the redirect off from the cold-started window works
but cannot delete the launcher while it runs: the IFEO value and state are gone at once. The helper
now says so honestly (`remove_launcher` returns whether the file is gone, no more "removed" in the log
while it exists): it tries to delete; a running image is renamed to `blink-tm.old.exe` inside the same
admin-only folder and marked for deletion at reboot (`MoveFileExW(DELAY_UNTIL_REBOOT)`, best effort;
`install_launcher` and the next removal also delete a leftover `.old`). If even the rename fails the
log says "kept (it is running), removed on the next switch-off or uninstall" and Settings shows
`Status::PendingRemoval` as long as the file is at the launcher path. Its text promises nothing that is not
scheduled: "blink-tm.exe is still present in Program Files... Switch Blink Task Manager on and off again, run
`install.ps1 -Uninstall`, or delete it as administrator" (the Settings block is sized for the ~5 lines, no ellipsis).
`install.ps1` (`Clear-TaskManagerRedirect`, so `-Uninstall` and `-UninstallTaskManager`) stops a running
`blink-tm*.exe` matched by its FULL path under `%ProgramFiles%\Blink` (a cold-started Blink IS that image) before
deleting, and prints what it could not delete instead of "turned off".

**Failure policy.** One attempt per request, no retry, no restart loop: every failure ends in the
native Task Manager (`launch` in `logic.rs`: untrusted launcher, hung Blink, start failure, no
window in 3 s, Shift). A Blink that crashes on start costs the user up to 3 s plus the native Task
Manager on each request, nothing more. A hung Blink cannot be replaced (a second one would hand over
to it), so it goes native too.

### 7.4 Escape hatch

- **Shift at launch** (without Ctrl, so Ctrl+Shift+Esc is not misread) → native Task Manager,
  handled inside the launcher, so it works even when Blink is broken, hung or uninstalled.
- **Tray / Settings → "Open Windows Task Manager"** → the same `DEBUG_PROCESS` bypass, so the
  native tool is always one click away while Blink runs.
- **Documented last resort** (in the UI's error text and this doc): delete the
  `IFEO\taskmgr.exe` `Debugger` value in an elevated regedit. The app must never require this, but
  it must document it, because AV quarantine of the launcher can make every route dead.

### 7.5 State ownership, update, uninstall

| Event | Required behaviour |
|---|---|
| Blink updates itself | Launcher is a different file, untouched. Blink re-checks the value/file on start. |
| Launcher update | Staged copy + hash check, then swap; keep `blink-tm.old.exe`. If the swap or the follow-up probe fails, restore the old file and re-probe; if both fail, **remove the value** (native TM) and report. Never leave the value pointing at a missing file. Done (§15): helper op `--taskmgr-refresh`, source = the helper's own image, no path argument. |
| Blink uninstalled (per user) | Per-user uninstall must **not** delete machine state silently. It offers "Restore Windows Task Manager" (UAC). If declined, the value stays and every Task Manager launch falls back to native TM because Blink is gone — safe, with a warning shown during uninstall. |
| Machine-wide removal | Settings → Restore (UAC), `blink.exe --taskmgr-disable`, or `install.ps1 -UninstallTaskManager` for admins. |
| Uninstall from Windows (Installed apps) or Settings → Uninstall Blink… (all users) | Both run `blink.exe --uninstall` (UAC first for an all-users install). It runs `taskmgr::uninstall_gate` before anything is removed: a redirect that is on is turned off (UAC); cancelled or failed = nothing removed. |
| UAC cancelled | Nothing is written/changed; UI says "still off" (enable) or "still enabled" (disable). No partial state. |
| Windows reset the value | Tray state reads "off (Windows reset it)" with a one-click re-enable. Never silently re-add (that would be a persistence fight). |
| Value points at another tool | Show "managed by another debugger (not Blink)" and never touch it. |
| Blink absent/corrupt/wrong version | Launcher still falls back to native TM. It does not repair machine state without consent. |

Invariants worth encoding as tests:

1. The `Debugger` value is only ever written in Program Files paths.
2. Write order: file present and verified → probe → `Enabled=1`. Removal order: value restored /
   deleted → `Enabled=0` → file may be deleted.
3. A failed enable leaves the previous value exactly as it was.
4. A failed launcher update leaves a working launcher and a working value, or no value.
5. The launcher never mutates registry state, never starts a user-writable exe, never prompts.
6. Per-user install/uninstall never touches machine-wide Blink state without an explicit UAC
   action.
7. All launcher paths are absolute, non-reparse, admin-only-writable; verified on every start
   (cheap) and on every probe.

---

## 8. Viability

| Mechanism | Routes covered | Admin | Scope | Fallback if Blink broken | Trust boundary | Precedent | Verdict |
|---|---|---|---|---|---|---|---|
| **A. IFEO → protected launcher → running Blink** | all image-name routes (Ctrl+Shift+Esc, Win+X, shortcuts, `taskmgr`) | yes, once (+ for repair/removal) | machine-wide | yes (native TM via `DEBUG_PROCESS`), unless the launcher itself is broken | good if Program Files + ACL checks; launcher elevated but only messages/launches System32 | PE + System Informer | **viable, not provably safe; §7** |
| B. IFEO → Blink directly | same | yes | machine-wide | no (Blink crash/hang = no TM; self-update leaves a window with no target) | bad if per-user install (EoP); good only with a machine-wide Blink | — | reject |
| C. IFEO → per-user `blink.exe` | same | yes | machine-wide | no | **unacceptable** (user-writable elevated target) | — | reject |
| D. `HKCU` App Paths for `taskmgr.exe` | none of the documented routes | no | per-user | n/a | same-user only | — | reject (System32 wins) |
| E. Replace/rename `taskmgr.exe` | n/a | n/a | machine | n/a | WRP/TrustedInstaller | — | impossible |
| F. Hotkey / shortcut / tray / Jump List only | its own key only | no | per-user | n/a | clean | ships today | **keep as the explicit launcher** |
| G. SDB shim `RedirectEXE` | probably all | yes | machine-wide | unknown | same as A | no primary doc for this use | not recommended |
| H. Keep native Task Manager, ship Blink launcher | — | no | per-user | native is the default | clean | — | **recommended now** |
| I. `DisableTaskMgr` policy | none | policy | per-user/policy | n/a | — | — | not a replacement |

---

## 9. Failure matrix

| Scenario | With §7 design | User sees | Recovery |
|---|---|---|---|
| Blink not running | native TM | Task Manager | tray hint if installed but not running |
| Blink hung (UI thread blocked) | `SendMessageTimeout` expires → native TM | Task Manager, ≤2 s later | Blink unaffected |
| Blink crashed after previous launch | native TM | Task Manager | Blink restarts on next tray/Start launch |
| Blink updated (exe swapped) | launcher unaffected; new Blink answers next time | normal | — |
| Blink elevated ("Restart as administrator") | launcher is same/higher IL → message allowed; window shows | Blink (elevated) | — |
| Two Task Manager requests at once | both message the same window; native fallbacks single-instance | one Blink window | — |
| User holds Shift | native TM | Task Manager | — |
| Launcher missing / quarantined | **no Task Manager**; the value points at nothing | "Windows cannot find …" | UAC/elevated regedit, Blink tray repair if Blink still runs |
| Launcher crashes on every run | same as above | no Task Manager | re-probe/repair; reinstall launcher |
| Value stale (moved launcher) | same | no Task Manager | repair action |
| Another tool's `Debugger` value | Blink reports "managed by another debugger", does not touch it | native or other tool | manual |
| UAC cancelled at enable | nothing written | "still off" | retry |
| UAC cancelled at disable/repair | value unchanged | "still enabled" | retry |
| Windows feature update removes the value | native TM | Task Manager | tray says "off (reset)"; one-click re-enable (UAC) |
| Windows changes Task Manager (packaged app, renamed exe) | value no longer matches | native TM | feature silently stops; detect and report |
| Explorer dead | launcher does not need Explorer for native TM; Blink messaging needs its window (already running) | Task Manager | — |
| Safe mode (no Blink tray) | native TM | Task Manager | — |
| AV deletes launcher but leaves value | no Task Manager | error | manual removal; the app must document it |
| Standard user on a machine | cannot enable; read-only | feature unavailable | — |
| Per-user uninstall while enabled | value stays; native TM | Task Manager | admin removal offered |

---

## 10. Unproven assumptions and impossible guarantees

| # | Assumption | Status | How to prove it (test machine, headless where possible) |
|---|---|---|---|
| 1 | The IFEO substitute inherits `taskmgr.exe`'s auto-elevation token | [INFERRED] | Enable the value for a throwaway debugger that writes its token IL to a file and exits; trigger each route; inspect. No GUI needed. |
| 2 | Ctrl+Shift+Esc actually goes through `CreateProcess` and IFEO | [SRC] System Informer comment + PE docs, not run here | Same test machine probe with a dummy target on Ctrl+Shift+Esc. |
| 3 | Win+X → Task Manager behaves the same | [UNVERIFIED] | Same probe, menu route. |
| 4 | Ctrl+Alt+Del → Task Manager is covered | [UNVERIFIED] | Same probe; note the secure desktop. |
| 5 | `DEBUG_PROCESS` bypass works from an *unelevated* launcher | [DOC] blog says yes; System Informer routes via a service on Win 8+, cause unexplained | Probe from both an elevated and an unelevated launcher. |
| 6 | A `DEBUG_PROCESS` fallback Task Manager is fully functional (not "debug build" weirdness) | [INFERRED] | Launch the bypass fallback on test machine and use it (GUI check). |
| 7 | The launcher can message an elevated Blink (if Blink was restarted as admin) | [INFERRED] UIPI allows high→low; same IL here | Test both IL combinations. |
| 8 | `SendMessageTimeout` reliably detects a hung Blink UI thread | [INFERRED] | Block Blink's UI thread in a test build; measure. |
| 9 | Feature updates keep or drop the value | [UNVERIFIED] community reports only | Observe after the next servicing update. |
| 10 | Defender/EDR reaction to an unsigned tool writing this value | [UNVERIFIED] (see the Defender review notes §2) | Defender review + event log after a test, as with every persistence-adjacent change. |

Impossible guarantees, stated plainly:

- **No Task Manager is reachable if the launcher cannot run.** No amount of fallback code helps,
  because the fallback is code in the launcher.
- **No guarantee the setting survives Windows servicing or third-party cleanup tools.**
- **No guarantee of a silent, prompt-free experience for enable/disable/repair**: those are
  admin operations.
- **No guarantee that an unsigned binary doing this is not flagged** by Defender/EDR.
- **No guarantee that a copied/renamed `taskmgr.exe` keeps its features** — the opposite is
  reported, and it is not a supported deployment.

---

## 11. Decisions needed before any code

1. **Accept a machine-wide component and admin at enable time?** That is the price of route
   coverage. Without it, only F/H are available.
2. **Resident-only, or cold start?** *(Resolved 2026-11-18: cold start from the protected image, §7.3a; the original reasoning follows.)* Resident-only (recommended) gives the strongest fallback and
   no silent elevation; cold start needs Explorer COM (unreliable when Explorer is dead) or a
   token/de-elevation path, plus the helper exit-code taxonomy this design otherwise avoids.
3. *(Resolved: no; Ctrl+Shift+Esc starts Blink, elevated, and falls back to native on any failure.)* **Is "native Task Manager when Blink isn't running" acceptable UX**, or must Ctrl+Shift+Esc
   always start Blink (requires cold start and its risks)?
4. **Shift-at-launch plus tray "Open Windows Task Manager"** as the escape hatch — agreed?
5. **Is the EDR/Defender alert noise acceptable** for an unsigned IFEO writer? If not, do not ship
   the replacement.
6. **If the launcher is ever quarantined/deleted, is a documented elevated `reg delete` an
   acceptable last resort?** If not, the replacement should not be shipped at all.
7. **Do we want this now?** The evidence says the mechanism works and the safe design exists, but
   the current per-user install cannot host it, the elevation behaviour is unproven, and native
   Task Manager is currently never broken. Option H costs nothing and keeps the risk at zero.

---

## 12. Sources

Microsoft documentation / Microsoft-authored:

1. GFlags Overview (the `Debugger` IFEO setting) — <https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/gflags-overview>
2. Debugging a Service (the IFEO key and `Debugger` value) — <https://learn.microsoft.com/en-us/windows/win32/services/debugging-a-service>
3. Process Explorer help, "Replace Task Manager" (shipped `procexp.chm`; mirror) — <https://documentation.help/procexp/documentation.pdf>
4. Process Explorer download page — <https://learn.microsoft.com/en-us/sysinternals/downloads/process-explorer>
5. Raymond Chen, "Image File Execution Options just inserts the debugger in front of the command line" — <https://devblogs.microsoft.com/oldnewthing/20070702-00/?p=26193>
6. Mithun Shanbhag (MSDN blog), "Image File Execution Options (IFEO)" (DEBUG_PROCESS bypass, bitness, wrong path behaviour) — <https://web.archive.org/web/20200302070123/https://blogs.msdn.microsoft.com/mithuns/2010/03/24/image-file-execution-options-ifeo/>
7. Process Creation Flags (`DEBUG_PROCESS`, `DEBUG_ONLY_THIS_PROCESS`) — <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
8. `DebugSetProcessKillOnExit` (kill-on-exit default; detach) — <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-debugsetprocesskillonexit>
9. `DebugActiveProcess` / `DebugActiveProcessStop` (detach semantics) — <https://learn.microsoft.com/en-us/windows/win32/api/debugapi/nf-debugapi-debugactiveprocess>
10. Application Registration (`App Paths`, documented search order) — <https://learn.microsoft.com/en-us/windows/win32/shell/app-registration>
11. `RegisterHotKey` — <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerhotkey>
12. Keyboard shortcuts in Windows (Ctrl+Shift+Esc, Win+X) — <https://support.microsoft.com/en-us/windows/keyboard-shortcuts-in-windows-dcc61a57-8ff0-cffe-9796-cb9706c75eec>
13. ADMX_CtrlAltDel `DisableTaskMgr` (blocks, does not redirect) — <https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-ctrlaltdel>
14. Application manifests, `autoElevate` — <https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests>
15. "Inside Windows 7 User Account Control" (Task Manager is auto-elevated; sigcheck dump) — <https://learn.microsoft.com/en-us/previous-versions/technet-magazine/dd822916(v=msdn.10)>
16. UAC architecture (`ShellExecute` → AppInfo → `CreateProcess`) — <https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/architecture>
17. Windows Resource Protection (protected system files, TrustedInstaller) — <https://learn.microsoft.com/en-us/windows/win32/wfp/about-windows-file-protection>
18. Supported Resource Replacement Mechanisms — <https://learn.microsoft.com/en-us/windows/win32/wfp/supported-file-replacement-mechanisms>
19. `AllowSetForegroundWindow` (letting the messaged Blink take the foreground) — <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-allowsetforegroundwindow>
20. MITRE ATT&CK T1546.012, Image File Execution Options Injection — <https://attack.mitre.org/techniques/T1546/012/>

First-party source (System Informer, commit `cb1af3a5858cbb20bf1e0786c882c8024fd52fe8`):

21. `SystemInformer/options.c` — IFEO key, quoted value, elevation shield, `%ProgramFiles%` warning, restore
22. `SystemInformer/main.c` — Shift-at-launch escape, Ctrl+Shift+Esc nuance
23. `SystemInformer/mainwnd.c` — "Start Task Manager" with IFEO bypass
24. `SystemInformer/appsup.c` — `PhCreateProcessIgnoreIfeoDebugger` (PEB flag, DEBUG flags, detach)
25. `SystemInformer/phsvc/svcapi.c` — elevated service route on Windows 8+

Local measurements (dev PC, 2026-11-17, read-only):

26. `taskmgr.exe` 10.0.26100.9818, 5,930,344 bytes; manifest `asInvoker` + `autoElevate=true`; owner
    `NT SERVICE\TrustedInstaller`; `SfcIsFileProtected` = true.
27. IFEO key ACL: Users `ReadKey`; Administrators/SYSTEM `FullControl`.
28. `%ProgramFiles%`: Users read/execute only.

Repo context: the Defender review notes §2 (IFEO risk, Defender surface), §4 (de-elevation options),
`docs/taskbar.md` (Jump List/tray surfaces), `src/win.rs` (registered `BlinkShowWindow`, single
instance, `ChangeWindowMessageFilterEx`), `src/setup/mod.rs` (per-user and per-machine install
paths), `src/update` (self-update swap).

---

## 13. What was not done

- No registry key or value was created, changed or deleted.
- No service, scheduled task, driver, hook or startup entry was installed.
- No user process, shortcut or shell configuration was modified.
- No GUI was launched on dev PC; nothing was run on test machine.
- No application code was written; this document is the only change.

---

## 14. Implementation notes (2026-11-18, branch `t3/ifeo-taskmgr-replacement`)

Built per §7 and §11, default off, Settings > TASK MANAGER. Choices and deviations:

- **Launcher = blink.exe under another name.** The helper copies the running exe to `%ProgramFiles%\Blink\blink-tm.exe`; `main` checks the file name first and runs only the launcher path (`src/taskmgr`). Not the ~50 KB separate binary of §7.1 (no second build target in a single-exe publish). It is as large as Blink and as old as the last enable or refresh (§15).
- ~~Resident-only~~ **Superseded by the cold start below** (§7.3a): Blink not running now means Blink is started from the protected launcher image; native Task Manager is the fallback.
- Staging targets `blink-surrogate-*.exe` exist only for test machine runs; the real target is refused from a portable copy.
- Probe: the helper starts the target with `--blink-self-test <nonce>`; the launcher checks trust and state, creates the native image suspended with the debug flags, terminates it unrun and exits 42. Anything else, or a 15 s timeout (the real image ran instead), rolls back.
- Not done: PEB `ReadImageFileExecOptions` clearing, a tray "Open Windows Task Manager" entry, the optional watcher. (Hash-checked launcher update: done, §15.)

**Accepted residual (review of the all-users installer):** `blink.exe --install-machine` and the Settings helper (`--taskmgr-enable/--taskmgr-disable`) are started elevated from whatever exe the user is running, which may sit in a user-writable folder. That is the trust of any downloaded installer (UAC shows the path); no Authenticode check. The uninstall gate refuses for a per-user install instead ("turn Task Manager off in Settings first").

### Snapshot before any real-key test (test machine only)

```
reg export "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options" <scratch>\ifeo-before.reg /y
Get-ChildItem "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options" | ? { $_.GetValue('Debugger') } | % { $_.PSChildName + '=' + $_.GetValue('Debugger') }
```
Keep an elevated prompt open on the test machine (the ssh session is already admin) so recovery never depends on Task Manager.

### Recovery (last resort, elevated cmd or PowerShell)

```
reg delete "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe" /v Debugger /f
reg delete "HKLM\SOFTWARE\Blink\TaskManager" /f
```
Works with no Blink and no Task Manager. If the launcher is gone and no admin prompt exists: Safe Mode or recovery-media regedit, same key. Normal route: Settings switch off (UAC), or `blink.exe --taskmgr-disable` elevated.

### Tests

`src/taskmgr/logic.rs`: toggle on/off with write order, idempotence, launcher kept while another target needs it, foreign value untouched, probe-failure and write-failure rollback, untrusted launcher writes nothing, Program-Files-only path, target whitelist, staging image match, real target touches only its own keys, status states, uninstall step, launcher bypass rules (Shift, Ctrl+Shift), recursion guard (`DEBUG_PROCESS` in flags, env guard, native path never the launcher), argv only a key, ACL policy and levels. All use an in-memory registry; `RealRegistry::new()` panics under `cfg(test)`.

### Review fixes (branch `t3/ifeo-taskmgr-fixes`)

Security review of the first cut, all fixed:

1. **`install.ps1 -Uninstall`** deleted `%ProgramFiles%\Blink` (with `blink-tm.exe`) but left `IFEO\taskmgr.exe\Debugger`: no Task Manager for any user. It now clears our value first through the new `Clear-TaskManagerRedirect` (also `-UninstallTaskManager` on its own): pure registry work in the 64-bit view, never runs an exe, touches only a value that is ours, and aborts the whole uninstall (exit 1, nothing removed) when it cannot clear it or is not elevated. Another tool's `Debugger` is left alone.
2. **Ours = recomputed path or recorded path.** `is_ours_any` also accepts the `LauncherPath` in our admin-owned state (it must be a `blink-tm.exe`), so a moved Program Files or an older install never reads as Foreign. Status checks the file the value really points at; disable removes both launchers; uninstall turns it off.
3. **No log from elevated code.** The helper and launcher install used `%LOCALAPPDATA%\Blink\blink.log` (user-writable, link-able). In `src/taskmgr` every log call is skipped when the process is elevated; the helper's result is its exit code.
4. **Staging is debug-only.** `--target`, `--target-path` and `blink-surrogate-*` targets are refused in release builds (`helper_args_allowed`); a release launcher always uses `taskmgr.exe`. Release only ever writes `taskmgr.exe` -> the Program Files launcher.
5. **ACL chain before the copy.** `Host::precheck` walks every part of the launcher path that exists (folder, Program Files, drive) for owner, ACEs and links before anything is created or copied. A failed enable removes the copied launcher, `blink-tm.new/old.exe` and the empty folder (unless another redirect has state), and a failed copy/rename deletes its `.new`.
6. **Surrogate state cleanup (debug builds).** `--taskmgr-disable` without `--target` (what Settings runs) turns off the real target and every staging target with state (`disable_all`); the uninstall gate (`uninstall_needed`) looks at them too in debug builds. Release looks at `taskmgr.exe` only.
7. **Rollback is conservative.** Enable re-reads `Debugger` right before writing it (a value that appeared meanwhile is Foreign, never overwritten); the rollback re-reads and restores or deletes only a value equal to the one this run wrote, putting back our previous value (and previous state) if there was one. `PreviousDebuggerExisted` is `1` only when a value of ours was already there.
8. **Launcher.** Elevation is checked before anything is trusted: unelevated, it trusts nothing (no Show) and hands over to the native Task Manager; the dry run answers `EXIT_NOT_ELEVATED`. When the native start fails it shows a message box (a hidden launcher otherwise leaves the user with nothing).

New tests (logic.rs, in-memory registry): recorded launcher never Foreign, restore of our previous value, race (value appears before the write / after it), honest `PreviousDebuggerExisted`, precheck before copy, copied launcher removed on failed enable (kept while another redirect needs it), release arg gate, no elevated logging, staging cleanup and uninstall decision. 28 taskmgr tests in total (19 before).

### Review fixes 2 (branch `t3/ifeo-taskmgr-fixes2`)

1. **Rollback never deletes a launcher a restored value points at.** When enable fails over a `Debugger` of ours that had no state, the rollback puts that value back; `drop_launcher_if_unused` is skipped then (`prev.is_some()`), so the value never points at a deleted `blink-tm.exe`. A first-time failure still cleans up.
2. **Recorded launcher is trusted only when it is `%ProgramFiles%\Blink\blink-tm.exe`** (same path Blink would write today, no `..`), and only when `HKLM\SOFTWARE\Blink` and `HKLM\SOFTWARE\Blink\TaskManager` are admin-only (`Host::state_trusted`; owner SYSTEM/Administrators/TrustedInstaller, no other write/delete/WRITE_DAC/WRITE_OWNER allow ACE; a missing key is fine). Otherwise enable and disable refuse (`Fail::Untrusted`, exit 10), delete nothing and change nothing; status reports `Stale` with the reason. A Program Files that moved since enable is therefore no longer followed: the old value reads as foreign/untrusted and the old file is left alone. Recover by hand (below).
3. **`install.ps1` aborts on a lookalike.** A `Debugger` that mentions `blink-tm.exe` but is not exactly our quoted `%ProgramFiles%\Blink\blink-tm.exe` (extra arguments, another folder, unquoted) stops the uninstall (exit 1, nothing removed) with the manual `reg delete` line, instead of being called foreign and left pointing at a deleted file. Only the exact quoted path is ours; the recorded `LauncherPath` no longer widens that.
4. **Rollback restores all state values**: `SchemaVersion`, `Enabled`, `LauncherPath`, `BlinkPath`, `PreviousDebuggerExisted`, `NativePath` (deleted again when the old state had none).
5. **Recovery hint** in Settings is a raw-string constant (`RECOVERY_HINT`); the `\t` in `IFEO\taskmgr.exe` used to render as a tab. Doc item 1 above had the same slip.

Tests added: recorded-path rules (5 bad paths), writable state key, launcher kept while a restored value needs it, full state restore, hint string. 32 taskmgr tests.

### Cold start fixes (branch `t3/launcher-cold-start-fixes`, 2026-11-18)

Security-review fixes on top of the cold start (details in §7.3a "Clean start", "Data dir", "Secure
desktop", "Lock result", "Cross-integrity message channel", and the Defender review notes):
P1 env whitelist + known-folder `ProgramFiles` in `sensors.rs`; P2-2 secure-desktop check; P2-1 elevated
cold child: no log, no data dir behind a link (residual risk documented); P3-2 lock timeout never starts
Blink; P3-3 honest launcher removal + `Status::PendingRemoval`; P3-4/P3-1 docs. New tests:
`child_env_is_a_whitelist_and_every_folder_comes_from_the_system`, `only_the_default_desktop_is_interactive`,
`only_a_got_lock_proceeds`, `secure_desktop_goes_straight_to_native`, `lock_timeout_never_starts_blink`,
`running_launcher_is_reported_not_removed`, `refused_data_dir_cannot_be_opened` (the old child-env test is
replaced). One new import-feature: `Win32_System_StationsAndDesktops` (`GetThreadDesktop`,
`GetUserObjectInformationW`, both in user32).

### Cold start (branch `t3/launcher-cold-start-trusted-binary`, 2026-11-18)

Reverses the "Resident-only" bullet above. Design and reasoning: §7.3a. Code: `launch`, `LaunchHost`,
`Seen`, `cold_child`, `child_env`, `blink_window_ok` (now also accepts the launcher image) in
`src/taskmgr/logic.rs`; `RealLaunch`, `ColdLock`, `cold_child_here`, `parent_pid` in
`src/taskmgr/mod.rs`; `setup::user_folders()`. Settings text changed (switch subtitle and the "On" line
say Blink may be started as administrator from the protected copy).

Tests: 44 taskmgr tests (12 new): running Blink shown and nothing started; absent Blink cold-started
and waited for (3 s budget); hung Blink goes native with no second start; Shift and disabled never look
for Blink; untrusted launcher messages and starts nothing; start failure -> native (no wait/stop); no
window -> started process ended before native; native failure is the only failed exit and there is one
start per request; abort/self-test start nothing; cold child recognised by parent image only; child
environment drops `BLINK_*` and takes profile/path variables from the system; window-image rule accepts
the launcher image.

Recovery is unchanged (the registry lines above). A cold-started Blink holds `blink-tm.exe` open while
it runs: to remove the launcher by hand, close that Blink first.

Test machine verification (debug build, slot 2, surrogate `blink-surrogate-tm.exe` = copy of cmd.exe, elevated
interactive scheduled task, IFEO snapshot taken first; the real `taskmgr.exe` key never touched):
enable exit 0; **cold start** with no Blink: one `%ProgramFiles%\Blink\blink-tm.exe` process, window
titled Blink, command line is the quoted path only, parent = the launcher; **second launch**: still one
Blink, no native; **hung** (Blink suspended): native image started, no second Blink; **Shift**: native only,
no Blink; **child cannot show a window** (a foreign window of class `BlinkMain` owned by another process):
started child ended itself, launcher fell back to native, no Blink left; cold start again after that works;
disable exit 0 removed launcher, folder and state; IFEO Debugger snapshot identical before and after.
Not machine-verified: the `StartFailed` branch (unit-tested only; a write-locked launcher cannot even be
started by Windows), Ctrl+Shift+Esc / Win+X / real key, Defender behaviour of the self-spawn.


### Review fixes, small items (branch `t3/taskmgr-review-fixes`, 2026-11-18)
- PendingRemoval text and `install.ps1` uninstall: see §7.3a above (stops a running launcher image by full path, honest message).
- `lock_file_policy`: an EMPTY DACL is now `Recreate` (it denies everybody, admins included, so the open would fail and the launcher would go native for ever). Test updated.
- `ColdLock::acquire`: the lock is created with `READ_CONTROL` and its DACL is re-checked with `GetSecurityInfo` on the OPEN handle, not by path again (share mode 0 pins the file, the folder is admin-only). `lock_path()` returns `Err` (no lock, native) instead of a path under an empty Program Files.
- `osutil::cold_started()` has its own flag (`mark_cold_started`), no longer implied by `disable_log()`; `SDDL_REVISION_1` instead of a literal.

## 15. Launcher refresh (branch `t3/taskmgr-launcher-refresh`, 2026-11-19, review fixes the same day)

- **Stale check (forward only).** The exe embeds a build ordinal (`build.rs`: seconds since 1970 at build time; `BLINK_BUILD_ORDINAL` overrides it). Enable and refresh store it as `BuildOrdinal` in the admin-only state key. `logic::freshness`: equal sha256 = current; different sha256 and this Blink's ordinal larger than the launcher's (a launcher without a record counts as older) = stale; ordinal equal or smaller = `NotNewer`, never offered, and the helper refuses too (exit 6, "not newer", nothing touched). A rolled-back Blink cannot downgrade the launcher. Hashes and the state read run on a worker thread (`fresh_view` returns the last answer, cached by size+mtime); when the answer changes the worker posts `WM_APP_TASKMGR`, so an open Settings panel repaints at once. The ordinal is reproducible: `BLINK_BUILD_ORDINAL`, else `SOURCE_DATE_EPOCH`, else the commit time of git HEAD; only when none exists it falls back to the wall clock with a `cargo:warning` (never 0, which would lock refresh out). Two uncommitted builds of one commit share an ordinal, so a refresh between them is refused: set `BLINK_BUILD_ORDINAL` for such tests.
- **Settings.** Task Manager block, never a pop-up. Orange line plus a button: "The Task Manager launcher is an older build than this Blink." [Refresh launcher]; "The Task Manager launcher file is missing, so Task Manager cannot open until it is put back." [Repair launcher] (our value, file gone, `blink-tm.old.exe` present). A launcher NEWER than this Blink shows "The Task Manager launcher is newer than this Blink; nothing to do." (no button). The block keeps its height while a step runs (the button is disabled, not hidden). The cold-started launcher image shows a hint with no button when its launcher differs from the installed Blink (direction-neutral): "Quit this Blink, start it from the Start menu, then press Refresh launcher in Settings > Task Manager." (it compares itself with the file at the state's `BlinkPath`). A normal start that forwards to a running instance does not check; the installed Blink checks at every Settings open.
- **Swap protocol (`logic::swap_files` / `restore_files` over the `Fs` seam, `RealFs` in `mod.rs`).** The launcher file is never removed or renamed first: (1) COPY the launcher to `blink-tm.old.exe` (admin-only DACL, `LOCK_SDDL`), (2) ONE atomic `MoveFileExW(REPLACE_EXISTING)` of the staged file onto the launcher, (3) only for a running image (it cannot be replaced): rename it to `.old` (admin-only, `MOVEFILE_DELAY_UNTIL_REBOOT`), rename the staged file in. The gap of step 3 (launcher missing, `.old` there) is repaired: `--taskmgr-refresh` sees the missing file and puts a copy of `.old` back in one replace (retried 5 times, 300 ms apart), probes it, else removes the value (exit 14). Restore is also copy + one replace, never delete-then-rename. Unit tests crash the protocol after every operation (`Disk` fake with a crash point) and assert: the launcher exists at every point, or (running image) `.old` exists and the repair restores it.
- **Unique staged names.** `blink-tm.new.<pid>-<ms>.exe` (restore: `blink-tm.new.r...`): a refresh only swaps the file this run staged and hashed. After the swap the file at the launcher path is re-hashed and must equal the staged image, else restore; after a restore it must equal the previous image. Enable/disable/remove do not take the cold-start lock: unique names plus the hash checks replace it there (`remove_launcher` deletes every `blink-tm.new*.exe`).
- **Helper op** `--taskmgr-refresh` (`logic::refresh`): state usable and enabled, value ours, launcher trusted (or missing = repair), cold-start lock (busy = exit 15), sha compare (equal = no-op), build newer, staged copy + hash, swap, trust + probe + re-hash; failure = restore + probe again (exit 13) or value removed (exit 14). The probe waits 5 s for the terminated image before any file work.
- **Exit codes and texts.** 13 kept, 14 removed, 15 busy, 16 launcher file (`Fail::Io`), 17 registry (`Fail::Registry`); the generic text no longer says "see blink.log" (the elevated helper writes none). The helper stores the failure text (control characters stripped, one line, 400 characters) as `LastError` in `HKLM\SOFTWARE\Blink\TaskManager` (target independent, so a disable that deletes a target's state cannot take it away). It is cleared at the start of every step that gets as far as the exclusion (a step that cannot take it writes its own "another step is running" text); the asking Blink reads it only when the key passes `state_trusted` and puts it FIRST in the message, which Settings wraps to up to three lines. A failed enable or a removal can leave the parent key holding just this value until the next step clears it. The first version of this section claimed "swap failure with the old file in place: exit 13": wrong, that case is exit 16 (nothing changed).
- **One exclusion for every step (review S1).** Every mutating helper op (enable, disable, refresh/repair; the uninstall gate's disable runs the same helper, under `GATE`) takes a named mutex `Global\BlinkTaskMgrHelper` (DACL SYSTEM + Administrators, `LOCK_SDDL`) BEFORE it reads any state; 30 s, then exit 15. A user who pre-creates the name can only make steps time out. Inside `refresh` the launcher file is looked at only under the cold-start lock (whether it is missing = repair is decided there), and the post-success writes (`BuildOrdinal`, `BlinkPath`) happen only if the state is still enabled, so a disable never finds its key written to again. `repair_offer` requires the state to be enabled, like refresh. At the start of a step, `blink-tm.new*.exe` older than 10 minutes (a cut-short run) is swept. A helper that runs longer than 2 minutes keeps the `GATE` closed until it ends.
- **Piggyback.** An elevated installed Blink refreshes (or repairs) without a second UAC once after the window exists, only when its own exe passes `verify_admin_only` (the uninstall gate's rule) and the state key is trusted; a user-writable exe only gets the Settings button with its prompt. One attempt per (launcher sha, running sha) pair, stored as `RefreshTried` in the admin-only state key (under the same mutex, and only while the state is still enabled) before the attempt (no marker file in the user's data dir, nothing elevated written into `%LOCALAPPDATA%`); an elevated self-update forces one more; a missing launcher is repaired at every start. "Dismissed" = once per pair.
- **Residual (§7.5):** the helper copies from the exe it runs from, which may be user-writable when the user clicks the button (same trust as enable); only the silent path is restricted.
- **Tests:** release suite 960 pass, 15 ignored (941 before the first review, 957 before the second).
- **Test machine (real IFEO key, exported first and restored; builds with ordinals 100/200/300):** enable(100) 0, self-test 42, `BuildOrdinal` 100; refresh(200) 0 (launcher == 200, `.old` == 100, 42); refresh from build 100 over 200: exit 6, launcher stays 200, `LastError` "not newer"; same build: 0, no change; `.old` held open: exit 16, launcher stays, 42, `LastError` readable; cold-start lock held: exit 15 after 4 s; refresh(300) 0; kill gap simulated (launcher renamed to `.old`, value still there): refresh exits 0, launcher restored, 42; gap with no `.old`: exit 14, value removed; running cold-started launcher (interactive task running `taskmgr.exe`): refresh(300) exit 0, launcher == 300, `.old` == the running 200, 42. The Settings stale line, button and click were checked on the first version only (unelevated Blink, launcher older, layout correct, button refreshed it); the Repair and hint variants were not seen in the GUI.
- **the Defender review notes:** one new import, `SetNamedSecurityInfoW` (advapi32, same Authorization feature as the SDDL conversion already used for the cold-start lock), to give `blink-tm.old.exe` the admin-only DACL. `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` is the same function the updater uses.
- **Second review (builds 100/200/300; IFEO key and state restored):** refresh 200 exit 0; refresh from build 100 over 200 exit 6 (`LastError` read); `.old` held exit 16; step mutex held exit 15 after 30 s with its own `LastError`; simulated gap repaired (exit 0, self-test 42); a 30-minute-old `blink-tm.new.*.exe` swept, a fresh one kept; refresh and disable started together 4 times: both exit 0, end state Off, no launcher, no state key, no parent key (the disable-first interleaving was not seen on test machine; unit-tested).
