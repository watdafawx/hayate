# Installs Hayate. No services, no scheduled tasks, no self-elevation. Two registry keys:
# HKCU\Software\Classes\CLSID\{toast activator}\LocalServer32, so a click on a Hayate toast in the
# notification centre opens Hayate on Watch, and the Installed-apps entry (Settings > Apps) under
# ...\CurrentVersion\Uninstall\Hayate (HKLM with -Machine, else HKCU; its UninstallString is
# "<exe>" --uninstall). hayate.exe can also install itself (a portable copy
# shows an "Install Hayate" bar); src/setup/logic.rs tests that both write the same footprint
# (listed in docs/defender.md, Self-install).
#   pwsh -File install.ps1                 per-user: %LOCALAPPDATA%\Programs\Hayate
#   pwsh -File install.ps1 -Machine        per-machine: %ProgramFiles%\Hayate (run from an elevated terminal)
#   pwsh -File install.ps1 -Autostart      also start hidden at sign-in (per-user Startup shortcut)
#   pwsh -File install.ps1 -AdminDefault   Start-menu "Hayate" asks UAC (shows temperatures); "Hayate (no admin)" stays plain
#   pwsh -File install.ps1 -Uninstall      remove whatever is installed (both modes if present)
#   pwsh -File install.ps1 -UninstallTaskManager   turn Hayate's Task Manager redirect off (elevated; -Uninstall does this first)
#   pwsh -File install.ps1 -UpdateFrom <dir>   auto-update from a publish.ps1 drop folder
#                                          (run from the repo it defaults to <repo>\dist when that exists)
#   pwsh -File install.ps1 -NoUpdate       turn auto-update off
# Exe: hayate.exe next to this script, else the published dist\hayate.exe, else target\release\hayate.exe.
param(
    [string]$Exe = $(foreach ($c in "$PSScriptRoot\hayate.exe", "$PSScriptRoot\dist\hayate.exe", "$PSScriptRoot\target\release\hayate.exe") { if (Test-Path $c) { $c; break } }),
    [switch]$Autostart,
    [switch]$AdminDefault,
    [switch]$Machine,
    [switch]$Uninstall,
    [switch]$UninstallTaskManager,
    # rig tests only: a staging image (hayate-surrogate-*.exe) instead of taskmgr.exe
    [string]$TaskManagerTarget = 'taskmgr.exe',
    [string]$UpdateFrom,
    [switch]$NoUpdate
)
$ErrorActionPreference = 'Stop'

$userDir    = Join-Path $env:LOCALAPPDATA 'Programs\Hayate'
$machineDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Hayate'
# pre-rename install folders and links (docs/rename-full.md): still cleaned up and stopped here
$legacyUserDir    = Join-Path $env:LOCALAPPDATA 'Programs\Blink'
$legacyMachineDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Blink'
$userMenu   = [Environment]::GetFolderPath('Programs')
$allMenu    = [Environment]::GetFolderPath('CommonPrograms')
$auto       = Join-Path ([Environment]::GetFolderPath('Startup')) 'Hayate.lnk'
$autoAll    = Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'Hayate.lnk'
$autoLegacy = Join-Path ([Environment]::GetFolderPath('Startup')) 'Blink.lnk'
$autoAllLegacy = Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'Blink.lnk'
# only a legacy folder that holds one of our exes is ours to remove
$legacyUserOurs = (Test-Path (Join-Path $legacyUserDir 'blink.exe')) -or (Test-Path (Join-Path $legacyUserDir 'blink-tm.exe'))
$legacyMachineOurs = (Test-Path (Join-Path $legacyMachineDir 'blink.exe')) -or (Test-Path (Join-Path $legacyMachineDir 'blink-tm.exe'))
$dataDir    = if ($env:HAYATE_DATA_DIR) { $env:HAYATE_DATA_DIR } elseif ($env:BLINK_DATA_DIR) { $env:BLINK_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'Hayate' }   # same overrides as hayate.exe
$updateIni  = Join-Path $dataDir 'update.ini'
$isAdmin    = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
                  [Security.Principal.WindowsBuiltInRole]::Administrator)

function Stop-Installed([string]$file) {
    # hayate.old.exe / blink.old.exe: the previous copy, still finishing a self-update
    $old = Join-Path (Split-Path $file) 'hayate.old.exe'
    $legacyOld = Join-Path (Split-Path $file) 'blink.old.exe'
    $p = Get-Process hayate, hayate.old, blink, blink.old -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $file -or $_.Path -eq $old -or $_.Path -eq $legacyOld -or $_.Path -eq ($file -replace 'hayate\.exe$', 'blink.exe') }
    if ($p) { $p | Stop-Process -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 300 }
    # an elevated Hayate cannot be stopped (or even seen) from a normal terminal: its exe stays locked
    if (Test-Path $file) {
        try { [IO.File]::Open($file, 'Open', 'ReadWrite', 'None').Dispose() }
        catch { throw "Hayate is still running from $file (probably elevated). Exit it from its tray menu, then run this again." }
    }
}

# $RunAsAdmin sets the shell-link SLDF_RUNAS_USER flag: bit 0x20 of byte 0x15 in the
# .lnk header (LinkFlags, 0x00200000). The shell then asks for UAC on every click.
#
# $AppId stamps System.AppUserModel.ID on the link (the shortcut's own property
# store; no registry). Hayate sets the same ID on its process, so Windows ties
# its toasts, its Settings > Notifications entry and its taskbar button to this
# shortcut: name "Hayate", Hayate's icon.
$appId = 'Hayate.TaskManager'   # keep in sync with src/notify.rs AUMID
# COM class Windows starts for a click on a toast in the notification centre (keep in sync with
# src/setup/logic.rs CLSID_STR). Hayate.lnk carries it as System.AppUserModel.ToastActivatorCLSID.
$toastClsid = '{8F49C037-2A7A-47A4-A7A0-715C9CF629B6}'
$clsidKey   = "HKCU:\Software\Classes\CLSID\$toastClsid"
# Installed apps entry (src/setup/logic.rs ARP_KEY / arp_values): the same values the in-app installer writes
$arpPath    = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Hayate'
# the key name before the Hayate rename: winget keys on the key NAME and matched "Blink" to prayag17.Blink.Alpha
$arpLegacy  = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Blink'
$arpVersion = '0.2.1'   # keep in sync with Cargo.toml (a unit test compares)
# Always the 64-bit registry view (where Settings > Apps reads it), whatever the PowerShell host's bitness.
function Open-ArpHive([bool]$Hklm) {
    $h = if ($Hklm) { [Microsoft.Win32.RegistryHive]::LocalMachine } else { [Microsoft.Win32.RegistryHive]::CurrentUser }
    [Microsoft.Win32.RegistryKey]::OpenBaseKey($h, [Microsoft.Win32.RegistryView]::Registry64)
}
# Removes the entry of an install in $Dir: the current key, and the legacy `Blink` key only when it is ours
# (DisplayName Blink/Hayate, InstallLocation = $Dir, UninstallString starts "$Dir\blink.exe"). Another program's key is never touched.
function Remove-Arp([bool]$Hklm, [string]$Dir) {
    try { $b = Open-ArpHive $Hklm; $b.DeleteSubKeyTree($arpPath, $false); $b.Close() } catch { }
    Remove-ArpLegacy $Hklm $Dir
}
function Remove-ArpLegacy([bool]$Hklm, [string]$Dir) {
    try {
        $b = Open-ArpHive $Hklm
        try {
            $k = $b.OpenSubKey($arpLegacy)
            if (-not $k) { return }
            try {
                $n = [string]$k.GetValue('DisplayName', ''); $l = [string]$k.GetValue('InstallLocation', ''); $u = [string]$k.GetValue('UninstallString', '')
            } finally { $k.Dispose() }
            $d = $Dir.Trim().TrimEnd('\').Replace('/', '\')
            $ours = ($n -in 'Blink', 'Hayate') -and ($l.Trim().TrimEnd('\').Replace('/', '\') -eq $d) -and $u.Trim().Replace('/', '\').StartsWith("`"$d\blink.exe`"", [StringComparison]::OrdinalIgnoreCase)
            if ($ours) { $b.DeleteSubKeyTree($arpLegacy, $false) }
        } finally { $b.Close() }
    } catch { }
}
function Set-LinkAppId([string]$path, [string]$id, [string]$clsid) {
    if (-not ('HayateInstall.Lnk' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace HayateInstall {
    [ComImport, Guid("0000010b-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPersistFile {
        void GetClassID(out Guid id);
        [PreserveSig] int IsDirty();
        void Load([MarshalAs(UnmanagedType.LPWStr)] string file, uint mode);
        void Save([MarshalAs(UnmanagedType.LPWStr)] string file, bool remember);
        void SaveCompleted([MarshalAs(UnmanagedType.LPWStr)] string file);
        void GetCurFile([MarshalAs(UnmanagedType.LPWStr)] out string file);
    }
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct PropertyKey { public Guid fmtid; public uint pid; }
    [StructLayout(LayoutKind.Explicit, Size = 24)]
    struct PropVariant { [FieldOffset(0)] public ushort vt; [FieldOffset(8)] public IntPtr p; }
    [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPropertyStore {
        void GetCount(out uint n);
        void GetAt(uint i, out PropertyKey key);
        void GetValue(ref PropertyKey key, out PropVariant v);
        void SetValue(ref PropertyKey key, ref PropVariant v);
        void Commit();
    }
    [ComImport, Guid("00021401-0000-0000-C000-000000000046")]
    class ShellLink { }
    public static class Lnk {
        // System.AppUserModel.ID (pid 5) and, when given, ToastActivatorCLSID (pid 26)
        public static void SetAppId(string path, string id, string clsid) {
            object link = new ShellLink();
            try {
                ((IPersistFile)link).Load(path, 2 /* STGM_READWRITE */);
                var fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3");
                var key = new PropertyKey { fmtid = fmtid, pid = 5 };
                var v = new PropVariant { vt = 31 /* VT_LPWSTR */, p = Marshal.StringToCoTaskMemUni(id) };
                try { ((IPropertyStore)link).SetValue(ref key, ref v); }
                finally { Marshal.FreeCoTaskMem(v.p); }
                if (!string.IsNullOrEmpty(clsid)) {
                    var k2 = new PropertyKey { fmtid = fmtid, pid = 26 };
                    var c = new PropVariant { vt = 72 /* VT_CLSID */, p = Marshal.AllocCoTaskMem(16) };
                    try { Marshal.Copy(new Guid(clsid).ToByteArray(), 0, c.p, 16); ((IPropertyStore)link).SetValue(ref k2, ref c); }
                    finally { Marshal.FreeCoTaskMem(c.p); }
                }
                ((IPropertyStore)link).Commit();
                ((IPersistFile)link).Save(null, true);
            } finally { Marshal.ReleaseComObject(link); }
        }
    }
}
'@
    }
    [HayateInstall.Lnk]::SetAppId($path, $id, $clsid)
}

function New-Link([string]$path, [string]$target, [string]$arguments, [string]$description, [switch]$RunAsAdmin, [string]$AppId, [string]$ToastClsid) {
    # fresh file: CreateShortcut on an existing link keeps its old properties (app ID, flags)
    Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    $s = (New-Object -ComObject WScript.Shell).CreateShortcut($path)
    $s.TargetPath = $target
    $s.Arguments = $arguments
    $s.WorkingDirectory = Split-Path $target
    $s.Description = $description
    $s.IconLocation = "$target,0"
    $s.Save()
    if ($AppId) {
        # a link without the ID still starts Hayate; only the toast/taskbar identity is weaker
        try { Set-LinkAppId $path $AppId $ToastClsid } catch { "Note: could not set the app ID on ${path}: $($_.Exception.Message)" }
    }
    if ($RunAsAdmin) {
        $bytes = [IO.File]::ReadAllBytes($path)
        $bytes[0x15] = $bytes[0x15] -bor 0x20
        [IO.File]::WriteAllBytes($path, $bytes)
    }
}


# Hayate's opt-in Task Manager redirect (Settings > Open Hayate instead of Task Manager) is
# HKLM\...\Image File Execution Options\taskmgr.exe\Debugger = "<ProgramFiles>\Hayate\hayate-tm.exe".
# Deleting that file without clearing the value leaves NO Task Manager for any user, so every
# uninstall clears our value first and ABORTS (nothing removed) when it cannot. Only a value that
# is ours (exactly the quoted hayate-tm.exe in Program Files) is touched; a value that mentions
# hayate-tm.exe in any other form aborts the uninstall; another tool's Debugger is left alone.
# Pure registry work: it never runs any exe.
function Clear-TaskManagerRedirect([string]$Target) {
    if ($Target -notmatch '^(taskmgr\.exe|hayate-surrogate-[a-z0-9._-]+\.exe)$') { throw "Not a target Hayate may redirect: $Target" }
    $ifeo  = 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\' + $Target
    $state = 'SOFTWARE\Hayate\TaskManager\' + $Target
    # the state record the pre-rename build wrote (recognised so an uninstall can remove it too)
    $legacyState = 'SOFTWARE\Blink\TaskManager\' + $Target
    $hklm  = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $k = $hklm.OpenSubKey($ifeo)
        $value = if ($k) { try { $k.GetValue('Debugger', $null) } finally { $k.Dispose() } } else { $null }
        $recorded = $null
        foreach ($s in @($state, $legacyState)) {
            $sk = $hklm.OpenSubKey($s)
            if ($sk) { try { $recorded = $sk.GetValue('LauncherPath', $null) } finally { $sk.Dispose() } }
            if ($recorded) { break }
        }
        $norm = { param($p) if ($p) { ([string]$p).Trim().Trim('"').Replace('/', '\').TrimEnd('\').ToLowerInvariant() } }
        $quoted = { param($v) $v -is [string] -and $v.Trim().StartsWith('"') -and $v.Trim().EndsWith('"') }
        # only the one place Hayate installs it counts as ours; a recorded path elsewhere is not trusted.
        # The pre-rename launcher (Program Files\Blink\blink-tm.exe) is accepted as a second exact form.
        $mine = & $norm ($machineDir + '\hayate-tm.exe')
        $legacyMine = & $norm ($legacyMachineDir + '\blink-tm.exe')
        $ours = ($value -is [string]) -and (& $quoted $value) -and (((& $norm $value) -eq $mine) -or ((& $norm $value) -eq $legacyMine))
        # A Debugger that mentions one of our launchers but is not exactly our quoted form (extra args, other
        # folder, unquoted, edited) may still point at it: calling it foreign and then deleting the file
        # would leave no Task Manager. Abort, nothing removed.
        if (-not $ours -and ($value -is [string]) -and ($value -match '(?i)(hayate-tm\.exe|blink-tm\.exe|\\Hayate\\|\\Blink\\)')) {
            throw "The Task Manager Debugger value ($value) mentions a Hayate launcher but is not exactly Hayate's own quoted entry, so Hayate will not guess. Nothing was removed. Fix or delete that value by hand: reg delete `"HKLM\$ifeo`" /v Debugger /f"
        }
        $sk = $hklm.OpenSubKey($legacyState)
        $legacyRecorded = if ($sk) { try { $sk.GetValue('LauncherPath', $null) } finally { $sk.Dispose() } } else { $null }
        if (-not $ours -and $value) { "Task Manager redirect ($Target) belongs to another tool: left alone."; return }
        $stateNow = $hklm.OpenSubKey($state)
        $hasState = [bool]$stateNow
        if ($stateNow) { $stateNow.Dispose() }
        if (-not $ours -and -not $recorded -and -not $hasState) {
            # no redirect state left, but a launcher left over from an earlier disable (it was running then) may remain
            $tm = @('hayate-tm.exe', 'hayate-tm.old.exe', 'hayate-tm.new.exe') | ForEach-Object { Join-Path $machineDir $_ }
            $tm += @('blink-tm.exe', 'blink-tm.old.exe', 'blink-tm.new.exe') | ForEach-Object { Join-Path $legacyMachineDir $_ }
            $any = @($tm | Where-Object { Test-Path -LiteralPath $_ })
            if ($any.Count -eq 0 -and -not (Test-Path -LiteralPath (Join-Path $machineDir '.coldstart.lock')) -and -not (Test-Path -LiteralPath (Join-Path $legacyMachineDir '.coldstart.lock'))) { return }   # nothing of ours
            if (-not $isAdmin) { throw "A leftover Hayate Task Manager launcher is in $machineDir or $legacyMachineDir. Removing it needs an elevated terminal. Nothing was removed." }
            $run = Get-Process -Name 'hayate-tm', 'hayate-tm.old', 'hayate-tm.new', 'blink-tm', 'blink-tm.old', 'blink-tm.new' -ErrorAction SilentlyContinue | Where-Object { $_.Path -and ($tm -contains $_.Path) }
            if ($run) { $run | Stop-Process -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 500 }
            Remove-Item -LiteralPath (Join-Path $machineDir '.coldstart.lock') -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath (Join-Path $legacyMachineDir '.coldstart.lock') -Force -ErrorAction SilentlyContinue
            $remain = @()
            foreach ($f in $tm) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue; if (Test-Path -LiteralPath $f) { $remain += $f } }
            foreach ($d in @($machineDir, $legacyMachineDir)) { if (-not (Get-ChildItem -LiteralPath $d -Force -ErrorAction SilentlyContinue)) { Remove-Item -LiteralPath $d -Force -ErrorAction SilentlyContinue } }
            if ($remain.Count) { "Left-over launcher: could not delete $($remain -join ', '). Exit it, then run this script again as administrator." } else { "Left-over Task Manager launcher removed." }
            return
        }
        if (-not $isAdmin) {
            throw "Hayate's Task Manager redirect is on (or half there). Turning it off needs an elevated terminal: open one with 'Run as administrator' and run this script again. Nothing was removed."
        }
        if ($ours) {
            $w = $hklm.OpenSubKey($ifeo, $true)
            try { $w.DeleteValue('Debugger', $false) } finally { $w.Dispose() }
            $again = $hklm.OpenSubKey($ifeo)
            $left = if ($again) { try { $again.GetValue('Debugger', $null) } finally { $again.Dispose() } } else { $null }
            if ($left) { throw "Could not clear the Task Manager Debugger value ($left). Nothing was removed. Recovery: reg delete `"HKLM\$ifeo`" /v Debugger /f" }
            $e = $hklm.OpenSubKey($ifeo)
            if ($e) {
                $empty = ($e.ValueCount -eq 0 -and $e.SubKeyCount -eq 0); $e.Dispose()
                if ($empty) { $hklm.DeleteSubKey($ifeo, $false) }
            }
        }
        $hklm.DeleteSubKeyTree($state, $false)
        # the pre-rename state goes only when it names our pre-rename launcher
        if ($legacyRecorded -and ((& $norm $legacyRecorded) -eq $legacyMine)) { $hklm.DeleteSubKeyTree($legacyState, $false) }
        $others = $false
        foreach ($rootName in @('SOFTWARE\Hayate\TaskManager', 'SOFTWARE\Blink\TaskManager')) {
            $root = $hklm.OpenSubKey($rootName)
            if ($root) { $none = ($root.SubKeyCount -eq 0 -and $root.ValueCount -eq 0); $root.Dispose(); if ($none) { $hklm.DeleteSubKey($rootName, $false) } else { $others = $true } }
        }
        # No redirect of ours is left: the launcher's cold-start lock goes too, and so does the folder when
        # nothing else is in it (a machine install keeps its folder). A Hayate the launcher cold-started IS the
        # hayate-tm.exe image (elevated), so it is stopped first, matched by its FULL path in %ProgramFiles%\Hayate only.
        $left = @()
        if (-not $others) {
            $tmFiles = @('hayate-tm.exe', 'hayate-tm.old.exe', 'hayate-tm.new.exe') | ForEach-Object { Join-Path $machineDir $_ }
            $tmFiles += @('blink-tm.exe', 'blink-tm.old.exe', 'blink-tm.new.exe') | ForEach-Object { Join-Path $legacyMachineDir $_ }
            $running = Get-Process -Name 'hayate-tm', 'hayate-tm.old', 'hayate-tm.new', 'blink-tm', 'blink-tm.old', 'blink-tm.new' -ErrorAction SilentlyContinue | Where-Object { $_.Path -and ($tmFiles -contains $_.Path) }
            if ($running) { $running | Stop-Process -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 500 }
            Remove-Item -LiteralPath (Join-Path $machineDir '.coldstart.lock') -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath (Join-Path $legacyMachineDir '.coldstart.lock') -Force -ErrorAction SilentlyContinue
            foreach ($f in $tmFiles) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue; if (Test-Path -LiteralPath $f) { $left += $f } }
            foreach ($d in @($machineDir, $legacyMachineDir)) { if ((Test-Path -LiteralPath $d) -and -not (Get-ChildItem -LiteralPath $d -Force -ErrorAction SilentlyContinue)) { Remove-Item -LiteralPath $d -Force -ErrorAction SilentlyContinue } }
        }
        if ($left.Count) {
            "Task Manager redirect ($Target) turned off, but could not delete: $($left -join ', ') (still running or locked). Exit it, then delete it as administrator or run this script again."
        } else {
            "Task Manager redirect ($Target) turned off."
        }
    } finally { $hklm.Dispose() }
}

if ($UninstallTaskManager) {
    Clear-TaskManagerRedirect $TaskManagerTarget
    return
}

if ($Uninstall) {
    # The redirect first: removing hayate-tm.exe while the IFEO value stays kills Task Manager for all users.
    # Cleared (or left, when it is another tool's) before anything is stopped or deleted; failure aborts.
    try { Clear-TaskManagerRedirect $TaskManagerTarget } catch {
        [Console]::Error.WriteLine("Hayate: $($_.Exception.Message)")
        exit 1
    }
    # Per-machine first: it needs admin, so check before removing anything at all.
    if ((Test-Path $machineDir) -or $legacyMachineOurs -or (Test-Path (Join-Path $allMenu 'Hayate.lnk')) -or (Test-Path (Join-Path $allMenu 'Blink.lnk'))) {
        if (-not $isAdmin) {
            [Console]::Error.WriteLine("Hayate is also installed per-machine in $machineDir. Removing it needs an elevated terminal.")
            [Console]::Error.WriteLine("Open a terminal with 'Run as administrator' and run this script again. Nothing was removed; this script never elevates itself.")
            exit 1
        }
    }

    Stop-Installed (Join-Path $userDir 'hayate.exe')
    Stop-Installed (Join-Path $machineDir 'hayate.exe')
    # a bridged 0.2.0 copy may still be the pre-rename blink.exe (a non-elevated start leaves the move pending)
    Stop-Installed (Join-Path $legacyUserDir 'blink.exe')
    Stop-Installed (Join-Path $legacyMachineDir 'blink.exe')
    # taskbar Jump List (Windows keeps it outside the install dir): the exe removes its own, headless, before it is deleted.
    # SECURITY: an elevated run must never execute the per-user exe (%LOCALAPPDATA% is writable by any process of that user,
    # so it could be swapped for attacker bytes that would then run as admin). Elevated: only the per-machine exe under
    # Program Files (admin-writable only). Not elevated: only the per-user exe (the user's own privileges).
    $jlExes = if ($isAdmin) { @(Join-Path $machineDir 'hayate.exe') } else { @(Join-Path $userDir 'hayate.exe') }
    # The startup manager switches Windows startup entries off and keeps the originals only in its own list
    # ($dataDir\startup.ini). Before ANYTHING is removed the same exe gives every one of them back (--startup-release,
    # exit code 0 = done). It runs under the same exe rule as above, so the elevated run never executes the per-user exe.
    # A failure (or no exe that may run it) stops the uninstall with nothing removed; this script never elevates itself.
    $iniFile = Join-Path $dataDir 'startup.ini'
    $hasOwned = (Test-Path -LiteralPath $iniFile) -and [bool](Select-String -LiteralPath $iniFile -Pattern '^own\.state=' -Quiet)
    $released = -not $hasOwned
    $steps = @('--startup-release', '--remove-jumplist')
    foreach ($exe in $jlExes) {
        if (Test-Path $exe) {
            $previousHayateMulti = [Environment]::GetEnvironmentVariable('HAYATE_MULTI')
            $previousBlinkMulti = [Environment]::GetEnvironmentVariable('BLINK_MULTI')   # pre-rename alias
            foreach ($step in $steps) {
                if ($step -eq '--startup-release' -and -not $hasOwned) { continue }
                $code = -1
                try {
                    [Environment]::SetEnvironmentVariable('HAYATE_MULTI', $null)
                    [Environment]::SetEnvironmentVariable('BLINK_MULTI', $null)
                    $proc = Start-Process -FilePath $exe -ArgumentList $step -Wait -PassThru -WindowStyle Hidden -ErrorAction Stop
                    $code = $proc.ExitCode
                } catch { } finally {
                    [Environment]::SetEnvironmentVariable('HAYATE_MULTI', $previousHayateMulti)
                    [Environment]::SetEnvironmentVariable('BLINK_MULTI', $previousBlinkMulti)
                }
                if ($step -eq '--startup-release') {
                    if ($code -ne 0) {
                        [Console]::Error.WriteLine("Hayate could not give every Windows startup entry it switched off back (exit code $code; details in $dataDir\hayate.log). Nothing was removed.")
                        [Console]::Error.WriteLine("Fix that first (an all-users entry needs an elevated terminal, a per-user one a normal terminal), or turn the entries back on in Task Manager > Startup apps, then run this again.")
                        exit 1
                    }
                    $released = $true
                }
            }
        }
    }
    if (-not $released) {
        [Console]::Error.WriteLine("Hayate has Windows startup entries switched off ($iniFile), and this terminal may not run the Hayate that can give them back.")
        [Console]::Error.WriteLine("An elevated terminal never runs the per-user hayate.exe. Run this from a normal (not elevated) terminal first. Nothing was removed.")
        exit 1
    }
    if ($isAdmin -and (Test-Path (Join-Path $userDir 'hayate.exe'))) {
        "Note: the per-user taskbar Jump List was NOT removed (an elevated run does not execute the per-user hayate.exe). Windows may keep a stale entry for the deleted per-user install; run '.\install.ps1 -Uninstall' once from a normal (not elevated) terminal BEFORE the elevated one to clear it."
    }
    # shortcuts of either generation (Hayate*/Blink*) in BOTH menus whose target is one of our exes, whichever install is being
    # removed (an all-users install can have legacy per-user links next to it); a link to anything else is left alone
    $ourExes = @((Join-Path $userDir 'hayate.exe'), (Join-Path $machineDir 'hayate.exe'), (Join-Path $legacyUserDir 'blink.exe'), (Join-Path $legacyMachineDir 'blink.exe'))
    # a pre-rename Startup link is only ours when it starts one of those
    if (Test-Path -LiteralPath $autoLegacy) {
        try {
            $t = (New-Object -ComObject WScript.Shell).CreateShortcut($autoLegacy).TargetPath
            if ($ourExes -contains $t) { Remove-Item -LiteralPath $autoLegacy -Force -ErrorAction SilentlyContinue }
        } catch { }
    }
    Remove-Item $auto -Force -ErrorAction SilentlyContinue
    if ($isAdmin) { Remove-Item $autoAll -Force -ErrorAction SilentlyContinue; Remove-Item $autoAllLegacy -Force -ErrorAction SilentlyContinue }
    $removed = @()
    foreach ($menu in $allMenu, $userMenu) {
        foreach ($n in 'Hayate.lnk', 'Hayate (admin).lnk', 'Hayate (no admin).lnk', 'Blink.lnk', 'Blink (admin).lnk', 'Blink (no admin).lnk') {
            $f = Join-Path $menu $n
            if (-not (Test-Path -LiteralPath $f)) { continue }
            try { $t = (New-Object -ComObject WScript.Shell).CreateShortcut($f).TargetPath } catch { continue }
            if ($t -and ($ourExes | Where-Object { $_ -ieq $t })) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue }
        }
    }
    if ((Test-Path $machineDir) -or $legacyMachineOurs -or (Test-Path (Join-Path $allMenu 'Hayate.lnk')) -or (Test-Path (Join-Path $allMenu 'Blink.lnk'))) {
        Remove-Item (Join-Path $allMenu 'Hayate.lnk'), (Join-Path $allMenu 'Hayate (admin).lnk'), (Join-Path $allMenu 'Hayate (no admin).lnk'), (Join-Path $allMenu 'Blink.lnk'), (Join-Path $allMenu 'Blink (admin).lnk'), (Join-Path $allMenu 'Blink (no admin).lnk') -Force -ErrorAction SilentlyContinue
        Remove-Item $machineDir -Recurse -Force -ErrorAction SilentlyContinue
        if ($legacyMachineOurs) {
            Remove-Item (Join-Path $legacyMachineDir 'blink.exe'), (Join-Path $legacyMachineDir 'blink.old.exe'), (Join-Path $legacyMachineDir 'blink.new.exe'), (Join-Path $legacyMachineDir 'blink.bad.exe'), (Join-Path $legacyMachineDir 'blink-tm.exe'), (Join-Path $legacyMachineDir 'blink-tm.old.exe'), (Join-Path $legacyMachineDir 'blink-tm.new.exe'), (Join-Path $legacyMachineDir '.coldstart.lock') -Force -ErrorAction SilentlyContinue
            if (-not (Get-ChildItem -LiteralPath $legacyMachineDir -Force -ErrorAction SilentlyContinue)) { Remove-Item -LiteralPath $legacyMachineDir -Force -ErrorAction SilentlyContinue }
        }
        $removed += "per-machine ($machineDir)"
    }
    if ((Test-Path $userDir) -or $legacyUserOurs -or (Test-Path (Join-Path $userMenu 'Hayate.lnk')) -or (Test-Path (Join-Path $userMenu 'Blink.lnk'))) {
        Remove-Item (Join-Path $userMenu 'Hayate.lnk'), (Join-Path $userMenu 'Hayate (admin).lnk'), (Join-Path $userMenu 'Hayate (no admin).lnk'), (Join-Path $userMenu 'Blink.lnk'), (Join-Path $userMenu 'Blink (admin).lnk'), (Join-Path $userMenu 'Blink (no admin).lnk') -Force -ErrorAction SilentlyContinue
        Remove-Item $userDir -Recurse -Force -ErrorAction SilentlyContinue
        if ($legacyUserOurs) {
            Remove-Item (Join-Path $legacyUserDir 'blink.exe'), (Join-Path $legacyUserDir 'blink.old.exe'), (Join-Path $legacyUserDir 'blink.new.exe'), (Join-Path $legacyUserDir 'blink.bad.exe'), (Join-Path $legacyUserDir 'blink-tm.exe'), (Join-Path $legacyUserDir 'blink-tm.old.exe'), (Join-Path $legacyUserDir 'blink-tm.new.exe'), (Join-Path $legacyUserDir '.coldstart.lock') -Force -ErrorAction SilentlyContinue
            if (-not (Get-ChildItem -LiteralPath $legacyUserDir -Force -ErrorAction SilentlyContinue)) { Remove-Item -LiteralPath $legacyUserDir -Force -ErrorAction SilentlyContinue }
        }
        $removed += "current user ($userDir)"
    }
    # the toast activator and the Installed-apps entry point at the install: they go with it (the only registry keys Hayate writes)
    Remove-Item $clsidKey -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Arp $false $userDir
    # the pre-rename key is ours only when it names the pre-rename folder/exe
    Remove-ArpLegacy $false $legacyUserDir
    if ($isAdmin) {
        Remove-Arp $true $machineDir
        Remove-ArpLegacy $true $legacyMachineDir
    }
    # auto-update settings point at the install: they go with it
    Remove-Item $updateIni, (Join-Path $dataDir 'update-state.ini'), (Join-Path $dataDir 'update-uac.txt') -Force -ErrorAction SilentlyContinue
    # exes parked by an in-app uninstall (Settings > Uninstall moves the running hayate.exe there)
    Remove-Item (Join-Path ([IO.Path]::GetTempPath()) 'Hayate-uninstalled') -Recurse -Force -ErrorAction SilentlyContinue
    # an emptied SOFTWARE\Hayate (or the pre-rename SOFTWARE\Blink) goes too, only when nothing is left in it
    if ($isAdmin) {
        foreach ($k in @('HKLM:\SOFTWARE\Hayate', 'HKLM:\SOFTWARE\Blink')) {
            $key = Get-Item -LiteralPath $k -ErrorAction SilentlyContinue
            if ($key -and $key.SubKeyCount -eq 0 -and $key.ValueCount -eq 0) { Remove-Item -LiteralPath $k -Force -ErrorAction SilentlyContinue }
        }
    }

    if ($removed) { "Removed: " + ($removed -join '; ') } else { "Nothing was installed." }
    "Its settings and log stay in $dataDir (delete by hand if you want)."
    return
}

if ($Machine -and -not $isAdmin) {
    [Console]::Error.WriteLine("Hayate: -Machine installs to $machineDir for all users and needs an elevated terminal.")
    [Console]::Error.WriteLine("Open a terminal with 'Run as administrator' and run this script again. This script never elevates itself.")
    exit 1
}
if (-not $Exe -or -not (Test-Path $Exe)) { throw "hayate.exe not found at '$Exe'. Build first: cargo build --release" }

$dir  = if ($Machine) { $machineDir } else { $userDir }
$menu = if ($Machine) { $allMenu } else { $userMenu }
$dest = Join-Path $dir 'hayate.exe'
$other = if ($Machine) { $userDir } else { $machineDir }
if (Test-Path $other) {
    "Note: Hayate is also installed in $other; run this script with -Uninstall first if you meant to move it (this run left it alone)."
}

Stop-Installed $dest
# a running pre-rename copy would keep its folder and could race the migration
Stop-Installed (Join-Path $legacyUserDir 'blink.exe')
Stop-Installed (Join-Path $legacyMachineDir 'blink.exe')
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $Exe $dest -Force
# version.txt (from publish.ps1) travels with the exe; leftovers of a self-update go
$ver = Join-Path (Split-Path $Exe) 'version.txt'
if (Test-Path $ver) { Copy-Item $ver (Join-Path $dir 'version.txt') -Force } else { Remove-Item (Join-Path $dir 'version.txt') -Force -ErrorAction SilentlyContinue }
foreach ($f in 'hayate.old.exe', 'hayate.new.exe', 'hayate.bad.exe', 'hayate.new.version.txt', 'version.old.txt') {
    Remove-Item (Join-Path $dir $f) -Force -ErrorAction SilentlyContinue
}

# auto-update source (Hayate reads it at start; no source = off)
if (-not $UpdateFrom -and -not $NoUpdate -and (Test-Path "$PSScriptRoot\.git") -and (Test-Path "$PSScriptRoot\dist")) {
    $UpdateFrom = "$PSScriptRoot\dist"
}
$updateNote = ''
if ($NoUpdate) {
    Remove-Item $updateIni -Force -ErrorAction SilentlyContinue
    $updateNote = 'auto-update off'
} elseif ($UpdateFrom) {
    $src = [IO.Path]::GetFullPath($UpdateFrom)
    New-Item -ItemType Directory -Force $dataDir | Out-Null
    Set-Content -Encoding utf8NoBOM $updateIni @(
        '; Hayate auto-update (written by install.ps1). Hayate reads it when it starts.',
        '; Turn off: set enabled = false (or delete this file, or run install.ps1 -NoUpdate), then restart Hayate.',
        '[update]',
        'enabled = true',
        "source = $src",
        "install_dir = $dir"
    )
    $updateNote = "auto-update from $src"
    if ($Machine) { $updateNote += ' (per-machine: only an elevated Hayate can update itself)' }
} elseif (Test-Path $updateIni) {
    $updateNote = "auto-update settings kept ($updateIni)"
}

# Only Hayate.lnk carries Hayate's app ID (and the toast activator). The second link gets an ID of
# its own: a link without one is matched to hayate.exe's ID anyway, and Windows then names the
# toasts after it. An elevated Hayate still maps to Hayate.lnk.
# shortcuts written before the rename to Hayate
foreach ($old in 'Blink.lnk', 'Blink (admin).lnk', 'Blink (no admin).lnk') { Remove-Item (Join-Path $menu $old) -Force -ErrorAction SilentlyContinue }
if ($AdminDefault) {
    Remove-Item (Join-Path $menu 'Hayate (admin).lnk') -Force -ErrorAction SilentlyContinue
    New-Link (Join-Path $menu 'Hayate.lnk')            $dest '' 'Hayate task manager (asks for UAC)' -RunAsAdmin -AppId $appId -ToastClsid $toastClsid
    New-Link (Join-Path $menu 'Hayate (no admin).lnk') $dest '' 'Hayate task manager (no admin)' -AppId "$appId.User"
} else {
    Remove-Item (Join-Path $menu 'Hayate (no admin).lnk') -Force -ErrorAction SilentlyContinue
    New-Link (Join-Path $menu 'Hayate.lnk')         $dest '' 'Hayate task manager' -AppId $appId -ToastClsid $toastClsid
    New-Link (Join-Path $menu 'Hayate (admin).lnk') $dest '' 'Hayate task manager (asks for UAC)' -RunAsAdmin -AppId "$appId.Admin"
}
# Toast activator: Windows runs "<install>\hayate.exe" -ToastActivated as the signed-in user (never
# elevated) for a click on a Hayate toast in the notification centre.
New-Item -Path "$clsidKey\LocalServer32" -Force | Out-Null
Set-Item -Path $clsidKey -Value 'Hayate toast activator'
Set-Item -Path "$clsidKey\LocalServer32" -Value "`"$dest`" -ToastActivated"
# Settings > Apps > Installed apps: "Hayate", its icon, and an Uninstall button that runs `hayate.exe --uninstall`
# (two native confirm boxes; an all-users install asks UAC first). HKLM for -Machine (this run is elevated), else HKCU.
$arpBase = Open-ArpHive ([bool]$Machine)
$arpKey = $arpBase.CreateSubKey($arpPath)
$sizeKb = [int][Math]::Min([int]::MaxValue, [Math]::Floor((Get-Item $dest).Length / 1024))
foreach ($e in @(
        @('DisplayName', 'Hayate', 'String'), @('DisplayVersion', $arpVersion, 'String'), @('Publisher', 'Hayate Project', 'String'),
        @('DisplayIcon', $dest, 'String'), @('InstallLocation', $dir, 'String'),
        @('UninstallString', "`"$dest`" --uninstall", 'String'),
        @('NoModify', 1, 'DWord'), @('NoRepair', 1, 'DWord'), @('EstimatedSize', $sizeKb, 'DWord'))) {
    $arpKey.SetValue($e[0], $e[1], [Microsoft.Win32.RegistryValueKind]$e[2])
}
$arpKey.Close(); $arpBase.Close()
# the pre-rename `Blink` key of this install goes after the new one is written (a crash in between leaves both: harmless)
Remove-ArpLegacy ([bool]$Machine) $dir
# an all-users install drops this account's own Installed-apps entry only when no per-user copy remains
# (this script leaves a per-user copy alone; the in-app "Replace my own copy" removes both)
if ($Machine -and -not (Test-Path (Join-Path $userDir 'hayate.exe'))) {
    Remove-Arp $false $userDir
}
# Always the plain exe: never elevated, never hidden from the user. -Machine: the all-users Startup folder
# (administrators only), like the in-app "Install for everyone".
$autoDst = if ($Machine) { $autoAll } else { $auto }
if ($Autostart) {
    New-Link $autoDst $dest '--resident' 'Hayate task manager (starts hidden at sign-in)'
} else {
    Remove-Item $autoDst -Force -ErrorAction SilentlyContinue
}
"Installed to $dir" + $(if ($updateNote) { " · $updateNote" } else { '' })
$(if ($AdminDefault) { "Start Menu: Hayate (asks for UAC), Hayate (no admin)" } else { "Start Menu: Hayate, Hayate (admin)" }) + $(if ($Autostart) { ' · starts hidden at sign-in' } else { '' })
