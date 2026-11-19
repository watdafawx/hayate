# Services tab: actions and startup type

Code: `src/svcctl.rs` (logic, SCM calls), `src/ui/services.rs` (list, menu),
`src/ui/svcprops.rs` (Properties panel), `src/win.rs` (`svc_menu`).

## What you can do

Right-click a service (or select it and use the toolbar):

| Item | Behaviour |
|---|---|
| Start / Stop / Restart | Enabled only when the service's state **as the list saw it** allows it. Stop and Restart need a service that accepts Stop. Disabled services cannot be started (item is off). |
| Startup type | Automatic, Automatic (delayed start), Manual, Disabled. The current one is checked. |
| Properties… | Blink's own panel (below). |
| Copy service name | The key name. |
| Restart Blink as administrator… | Shown only when Blink is not elevated. The one existing elevation path; Blink never prompts on its own. |
| Stop waiting for … | Shown while an action is pending. Stops *waiting*; the service is not killed. |

## Properties panel

Native Windows has **no supported way** to open the Services Properties page for
one service from another program. `mmc.exe` documents only
`<file>.msc [/a] [/64] [/32]`; there is no per-service switch, and selecting a row
by SendKeys, UI automation or registry tricks is not supported, so Blink does not
do it. Instead Blink draws its own panel and never calls it "Windows Properties":

- Read-only: display name, service name, description, program path, account,
  state/PID, dependencies.
- Startup type radio group, with an explicit **Apply** (nothing is written when the
  panel opens).
- **Open Windows Services** launches `%SystemRoot%\System32\mmc.exe
  %SystemRoot%\System32\services.msc` (absolute paths, ShellExecute `open`). It
  opens the whole list; find the service there.

## How a change is made

1. The service **key name**, the list's program path and its startup type are
   captured when the menu/panel opens (never a row index or display name). The row
   list is frozen while a menu is open. The menu items show the state **as the list
   saw it**; they are not a live check. The live check happens on every click
   (step 3).
2. **No baseline, no write.** A capture without a service name, a program path or a
   known startup type (for example a row the list could not read a path for) has
   nothing to be verified against. Its Start / Stop / Restart / startup items are
   off ("Actions off: the list has no program path for it yet"), Apply stays off,
   and the worker refuses at once with the reason. Properties still opens,
   read-only.
3. A worker thread opens the service **once** and keeps that one handle for the
   whole transaction (rights: `SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS` plus
   what the step needs, e.g. `SERVICE_CHANGE_CONFIG`, `SERVICE_START`,
   `SERVICE_STOP`). The identity read, the write and the read-back all go through
   that handle, so a service deleted and recreated under the same name in between
   cannot be the object that gets written: the handle stays bound to the object
   that was opened. Immediately before every Start and Stop dispatch it reads the
   configuration again **on the same handle** and fails closed unless the image
   path, startup type and (for Automatic) the delayed flag still match what the
   user saw. If not, it says what changed and does nothing. A Restart whose target
   was replaced after the stop says "was stopped, but not started again".
4. Startup write: `ChangeServiceConfigW` with every field `SERVICE_NO_CHANGE`/NULL
   except `dwStartType`, plus `ChangeServiceConfig2W(DELAYED_AUTO_START_INFO)` only
   when the delayed flag actually differs. Account, binary path, arguments,
   dependencies, description, recovery actions and triggers are never touched.
5. **Delayed flag unknown is an error, not "off".** If `QueryServiceConfig2W`
   fails, the flag is unknown: the panel warns, Apply stays off until a fresh read
   has it, and the worker refuses.
6. **A failed call may still have committed.** After a failed
   `ChangeServiceConfigW` Blink reads the service again before saying anything: if
   the new type landed it carries on; if unchanged it reports "not changed"; if the
   read fails or shows a third value it reports that actual state and does not
   retry. After a failed delayed-flag write it rolls the startup type back **only
   if the service is still exactly in the state Blink wrote** (startup type equal to
   Blink's write, delayed flag as before). Anything else is somebody else's newer
   change: it is left alone and reported. The two writes are two calls and are not
   atomic, so a partial result is possible; it is reported as such, and the result
   names the state the service is in. Which order Windows would accept them in is
   not documented, so Blink does not rely on one. A successful change is confirmed
   by a final read-back (delayed flag known, nothing else different).
7. **One work item per service, shared.** Every action (Start / Stop / Restart) and
   every startup change takes the same per-service guard. A second request for the
   same service is refused at once on the calling thread ("already busy: ...") and
   the menu shows "Changing its startup type…" or the running action while busy.
   The guard lives in a process-wide registry, not in the panel, so closing the
   panel or switching tab does not free it. Workers are capped at four changes
   and four reads; excess requests are refused, not queued. A worker that panics
   releases its guard and reports an error. The Properties panel stops waiting
   after 20 s for reads or 60 s for Apply, with an explicit unconfirmed result.
   This cannot cancel an SCM call. A permanently hung call holds its slot until
   Blink exits; four hung change calls prevent further changes until restart.
8. The Properties panel closes when the tab changes and releases its click areas.
   A change still in flight when it closes is reported in the Services header when
   it ends ("still being applied … the result will show here"); the panel never
   claims it was cancelled.

Rules kept on purpose:

- Stop never force-kills a process, never touches a shared svchost, and never stops
  dependents silently. If dependents block it (error 1051) Blink says so and stops;
  it does not stop them for you.
- Disabling a running service does not stop it. The panel and the result say so.
- A trigger-start annotation ("Automatic (trigger)", "Manual (trigger)") is display
  only; triggers are never deleted.
- Delayed flag on a non-Automatic type is explained ("no effect until Automatic").
- Windows service permissions decide whether a change is allowed. Blink does not
  guess: an unelevated Blink can legitimately succeed on a service whose DACL
  allows it, and gets the real Windows error on one that does not (restart as
  administrator if access is denied). No bypass, no privilege tokens, **no
  automatic elevation**. The only elevation path is the user-clicked "Restart Blink
  as administrator…".
- Automatic-start persistence is scoped to **existing services only**: Blink changes
  the startup type of a service that already exists. It creates no service or
  driver, writes no program path or account, and changes no ACL or dependency.
  Blink performs no direct registry write: the SCM persists startup settings in
  the service key's `Start` and `DelayedAutostart` values, which the list also reads.
- Managed-service policy is a separate control plane. Blink makes one explicit
  user-requested operation through documented SCM calls, reports refusal errors,
  and reads back the actual result. It never overrides policy or repeatedly
  re-applies a setting that policy changes later.
- No direct registry edits, service creation/deletion, driver loads or shell
  command strings built from service data.

## API delta (for security review)

Win32 calls, all documented SCM APIs: `OpenSCManagerW(SC_MANAGER_CONNECT)`,
`OpenServiceW` (one open per transaction, rights `SERVICE_QUERY_CONFIG`,
`SERVICE_QUERY_STATUS` plus `SERVICE_CHANGE_CONFIG` / `SERVICE_START` /
`SERVICE_STOP` / `SERVICE_ENUMERATE_DEPENDENTS` as the step needs),
`QueryServiceConfigW`, `QueryServiceConfig2W`, `QueryServiceStatusEx`,
`ChangeServiceConfigW`, `ChangeServiceConfig2W`, `ControlService(SERVICE_CONTROL_STOP)`,
`StartServiceW`, `EnumDependentServicesW`, and `ShellExecuteW("open", absolute
mmc.exe, absolute services.msc)` for "Open Windows Services".

`ShellExecuteW` does start a process: System32 `mmc.exe` opening `services.msc`,
on a user click. It runs with Blink's own token, so when Blink is elevated the MMC
it opens is elevated too (inherited, not requested). StartServiceW asks the SCM to
start the service's configured image under its configured account, which may be
LocalSystem. Blink does not construct or launch that command itself. No new
privileges, tokens, direct registry writes or network use.

## Testing

- Fake-SCM unit tests in `svcctl.rs` and the UI modules. The fake models service
  **objects** (a name table plus instances that can be deleted and recreated), so a
  handle stays bound to its object. Covered: replacement image path before Start
  and before Stop (the old code started the replacement), recreation between read
  and write, rollback only of Blink's own write (never a newer change), a write
  that commits but reports failure, an unreadable delayed flag, a capture with no
  image path, startup change versus action (shared guard, ordering), the worker
  cap, a panicking worker, the public entry points sharing one guard, panel
  timeouts, tab switch and close with a change in flight.
- Mutation checks (temporarily reverting one guard each) make the matching
  regression tests fail.
- Ignored real-SCM test `real_scm_startup_round_trip_changes_nothing_else`: set
  `BLINK_SVC_FIXTURE=blinkfixture...` to a disposable service the current user may
  configure. It refuses any other name.
- `tools/svcfixture` is a do-nothing **own-process** service. It answers to the name
  the SCM started it under (ServiceMain's first argument); the dispatcher-table
  name is ignored for own-process services (SERVICE_TABLE_ENTRYW docs), so nothing
  is hard-coded. Register it as `type= own` under a unique `BlinkFixture…` name,
  preferably as LocalSystem so no folder ACL has to change, and set a DACL on
  **that service only** (through the SCM) if a normal-user Blink must change it.
  Never alter the ACL of a shared or system folder. Delete the service and its
  folder when done. Debug hook
  `blink --test-svc <blinkfixture*> <start|stop|restart|auto|delayed|manual|disabled>`
  also refuses other names.
- Real-SCM proof for `1e083db`: see "Test machine verification" below.

## Test machine verification (2026-11-17, source `1e083db`)

Verifier used the existing admin channel on the test machine (`WindowsPrincipal
IsInRole(Administrator)` = True, `TESTPC\admin`). No ACL of any folder, scheduled
task, IFEO or Defender setting was changed, no real service was modified, no UAC
workaround was used. One fixture service, `BlinkFixtureA7q3k`, was created with
`sc.exe` (own-process, LocalSystem, Manual, absolute image
`<scratch>\blink-svcfixture-a7q3k\svcfixture.exe`, SHA-256
`390200BD7BE38782829CAF00847A65F51DC75CD00EA21E28E2E45C66BCD8826F`, built by
`cargo build --release` from `tools/svcfixture` in an isolated slot). Note: `<scratch>`
inherits Everyone:Full, so that image path is writable by any local user while a
LocalSystem service points at it; the fixture lived minutes, its hash was checked
at start of the proof and again before deletion, and it is deleted (do not copy this layout for anything that
lives longer).

| Check | Result |
|---|---|
| Fixture answers to the SCM name | The binary contains no copy of the name. SCM started it as `BlinkFixtureA7q3k`: reached RUNNING with PID, process image path and hash equal to the registered image, STOP honoured (STOPPED). |
| Ignored real-SCM test, release harness (`blink-464b09864419525b.exe`, SHA-256 `0869002BF03E…ECF7`) | `svcctl::tests::real_scm_startup_round_trip_changes_nothing_else` passed with `BLINK_SVC_FIXTURE=BlinkFixtureA7q3k` (Disabled, Delayed, Manual, Automatic, Disabled, Manual, restore; wrong-image and missing-name refusals). |
| SCM before/after (type, start, error control, image, account, dependencies, display name, description, `sc qfailure` recovery, `sc qtriggerinfo`, SDDL, registry values) | Identical except the start type and the `DelayedAutostart` registry value, which the startup writes create. |
| Debug `--test-svc` hook (debug exe `B22CAB6A…FFFE`), elevated headless | start, restart (new PID), stop all ok; auto, delayed, manual, disabled each read back through the SCM (`Start` 2/2+`DelayedAutostart`=1/3/4); disabling a running service left it running (message says so), then Stop stopped it. |
| Debug Blink GUI, unelevated, fixture DACL granted to the current user only (query/config/start/stop; restored afterwards) | Services tab found the fixture; right-click menu (Start enabled, Stop/Restart off while stopped); Properties showed name, description, program, LocalSystem, no dependencies, Apply off until a change; delayed start Applied and confirmed by the SCM; toolbar Start and Restart worked (new PID); Properties closed on tab switch; Open Windows Services started one `mmc.exe` child of Blink (no UAC prompt seen), which the verifier closed. |
| Error path | With the DACL restored to the default, a GUI Stop on the running fixture reported "access denied: Windows allows only administrator…", changed nothing. |
| Release candidate smoke | Release `blink.exe` (see hash in the hand-off) launched unelevated, Services tab, fixture row and menu shown. |

Not verified: elevated GUI (UIPI blocks the driver); the busy/hung-worker UI
paths and the 1051 dependents refusal on a real service (fake-SCM tests only);
recovery actions were queried (none configured), not exercised. At a narrow window
the Services header label overlaps the toolbar buttons and the footer text
overlaps (cosmetic).

Cleanup: fixture stopped, deleted from the SCM, its folder removed; slot 2 app
stopped; no fixture or Blink process left. The candidate folder
`<scratch>\blink-services-candidate-1e083db` is left for the parent's scans.

## Sources

- QueryServiceConfig / QueryServiceConfig2 / ChangeServiceConfig /
  ChangeServiceConfig2 / SERVICE_DELAYED_AUTO_START_INFO / ControlService /
  StartService / EnumDependentServices / Service Security and Access Rights
  (learn.microsoft.com, win32/services and win32/api/winsvc).
- SERVICE_TABLE_ENTRYW and LPSERVICE_MAIN_FUNCTION: own-process dispatcher name
  ignored; ServiceMain's first argument is the service name.
- Microsoft Management Console command-line options (`mmc` reference): no
  per-snap-in or per-service selection switch.
