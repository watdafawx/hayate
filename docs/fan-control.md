# Fan control feasibility for Hayate

Research date: 2026-11-17. Claims are tagged [S] sourced (URL), [O] observed on the
user's PC (FOX, read-only queries), or [I] inferred. Anything untagged is a design opinion.

## Summary (10 lines)

1. Recommendation: do NOT build native fan/pump control in Hayate now. Do (C) display-only first, then (B) a thin FanControl status/preset-switch integration only if it proves cheap.
2. Reason: this board is ASUS TUF GAMING B850-PRO WIFI7 W NEO, and ASUS's own fan service plus WMI already owns the fan headers [O].
3. No USB AIO (Corsair/NZXT/etc.) is visible [O]. The AIO is most likely on the AIO_PUMP/CPU_FAN headers, so liquidctl-style HID work would not help [I].
4. Native SuperIO write control needs a hand-written NCT67xx driver over PawnIO `LpcIO`; Hayate's own docs record that even read-only ITE support was dropped as unvalidated [S: docs/sensors.md].
5. Curves only work while a process runs. Enforcing them needs an always-on elevated background loop, which breaks Hayate's "costs ~0 when hidden" rule.
6. Risk of a bad write: fans stuck low or off, CPU overheating if Hayate crashes; no portable fail-safe exists across SuperIO/WMI [I].
7. Cost of display-only (C): ~0 extra when hidden, 1 s reads while the Vitals tab is painted; same pattern as existing sensors.
8. Cost of native (A): ~4-8 weeks, a new elevated resident daemon, a second driver client (writes), Defender/anti-cheat exposure. Too much.
9. (B) needs FanControl to run anyway; its CLI/profile API is undocumented [S]. Cheap to try, low value until verified.
10. Hardware blockers: Armoury Crate + AsusFanControlService + ASUS AsIO3/WinRing0 drivers already running [O]; two controllers fight [S].

---

## 1. Existing solutions

### FanControl (Rem0o)
- Closed source, free; the repo says "Sources for this software are closed" [S: https://github.com/Rem0o/FanControl.Releases]. Cannot reuse its code.
- Backend: LibreHardwareMonitor (LHM) for sensors/SuperIO; NvAPIWrapper for NVIDIA GPU fans; ADLXWrapper for AMD GPUs [S: same].
- AIOs are plugins: liquidctl-based, Corsair Commander/Hydro, NZXT Kraken, Aquacomputer, Gigabyte Waterforce [S: same].
- ASUS boards: only via the community plugin FanControl.AsusWMI (MIT), which "should work with any board using ASUSHW v3"; tested on Crosshair VII; AM5/TUF not mentioned [S: https://github.com/Mourdraug/FanControl.AsusWMI].
- Drivers: V238+ ships LHM built on PawnIO, replacing WinRing0 which Defender flagged (Trojan:Win32/Vigorf.A) [S: Rem0o README].
- Runs as a boot-time service possible; GUI profiles. No documented CLI/API for switching profiles in the README [S]. Fail-safe on exit/crash is not documented [S: not found].
- Known pain: HWiNFO and FanControl touching the same headers cause fans winding up/down [S: https://www.hwinfo.com/forum/threads/hwinfo64-taking-over-fancontrol-sensor-settings.8390/]. Sleep crash reports with the ADLX plugin [S: https://github.com/Rem0o/FanControl.Releases/discussions/1717].

### LibreHardwareMonitor
- MPL 2.0, open source [S: https://github.com/LibreHardwareMonitor/LibreHardwareMonitor]. Moved WinRing0 to PawnIO (PRs #1857, #1908) [S: search result for LHM release notes]. Has an NCT677x mutex PR (#1254) [S: same]. Reference code for NCT67xx register maps; reading it is the only practical way to port (MPL: file-level copyleft, porting logic to Rust is a derivative; keep notes of attribution) [I].
- Whether LHM's PWM write path is stable on NCT6799 was not confirmed in these sources [S: not found].

### liquidctl
- GPL-3.0 [S: https://github.com/liquidctl/liquidctl]. Supports Corsair Hydro/Elite Capellix/Commander, NZXT Kraken X/Z/2023+, Asetek 690LC (EVGA CLC, old Kraken), ASUS Ryujin II/III and Ryuo I, MSI MPG Coreliquid K360, Gigabyte, Lian Li, Aquacomputer [S]. iCUE Link not listed [S]. ASUS Ryujin III 360 reported as not detected [S: https://www.github.com/liquidctl/liquidctl/issues/677].
- Mostly HID, works with the Microsoft HID driver; some need WinUSB via Zadig [S].
- Settings are applied by commands, "initialized after every boot", some devices lose state on sleep; automatic control needs an external script/daemon [S].
- GPLv3: porting protocol code into Hayate makes Hayate's combined work GPL unless re-derived from the protocol docs/captures [I]. Check Hayate's license before any port.

### CoolerControl (Linux)
- GPL-3.0+, "Cooling device control for Linux" [S: https://gitlab.com/coolercontrol/coolercontrol]. Architecture (daemon + UI) not confirmed from the page [S: not found]. Useful only as a design reference: separate privileged daemon, UI client [I].

### HWiNFO
- Fan control is "not an official HWiNFO feature", only a few machines [S: https://hwinfo.com/forum/threads/fan-control.2241/]. A staff reply says ROG fan controllers may be supported on some boards [S: sourceforge reply cited in search]. Not an option.

### Argus Monitor, OpenRGB
- Argus Monitor: commercial, kernel driver based; details not confirmed in this research [S: not found]. OpenRGB: LED-focused, was affected by WinRing0 alerts [S: Microsoft/Neowin coverage below].

### ASUS (Armoury Crate / Fan Xpert / AI Suite)
- Fan Xpert in Armoury Crate covers AM5 boards [S: https://geekchamp.com/?p=680203 (third-party guide)].
- Armoury Crate's fan control overrides BIOS settings even when "off" in one report [S: https://rog-forum.asus.com/t5/armoury-crate/armoury-crate-v5-0-10-0-broken-fan-control-z690/m-p/897882]. Two tools on the same header fight [S: geekchamp above].
- ASUS AIOs: Ryujin III pump 4-pin can go on CPU_FAN, AIO_PUMP or W_PUMP+; Ryujin II pump needs the mobo USB2 header for Armoury Crate to read it [S: https://rog-forum.asus.com/t5/all-in-one-cooling/rog-ryujin-iii-360-extreme-radiator-fans-not-showing-up-in/td-p/1133589 and https://manuals.plus/m/0c5ed60f4f280e6d5bb76cf527fe9b8b4882600915bcdb2e81d171fea25238d0].

### GPU fan control (RTX 5070 Ti)
- NVML `nvmlDeviceSetFanSpeed_v2` needs root/admin; `SetDefaultFanSpeed_v2` is the reset, and one NVIDIA forum thread says it does not restore the auto curve [S: https://docs.nvidia.com/deploy/nvml-api/group__nvmlDeviceCommands.html and https://forums.developer.nvidia.com/t/nvmldevicesetdefaultfanspeed-v2-does-not-resume-fan-speed-algorithm-please-fix/214430]. RTX 50 + Windows support: not confirmed [S: not found]. FanControl does it through NvAPIWrapper (NVAPI) [S].
- Hayate already reads GPU fan RPM via D3DKMT [S: docs/sensors.md].

### Driver / blocklist / anti-cheat
- WinRing0 (CVE-2020-14979) flagged by Defender as VulnerableDriver:WinNT/Winring0; Windows 11 2022+ has the vulnerable-driver blocklist on by default [S: https://support.microsoft.com/windows/security/threat-malware-protection/microsoft-defender-antivirus-alert-vulnerabledriver-winnt-winring0, https://www.neowin.net/news/windows-1110-is-flagging-winring0-on-your-pc-monitoring-fan-control-apps-heres-why/].
- PawnIO is the replacement. Repo lists 28 modules including `LpcIO`, `SmbusNCT6793`, `LpcACPIEC`, `IsaBridgeEC`, `SmbusPIIX4`, `Nvidia`, `RyzenSMU`; no IT87 and no Nuvoton fan module by name; LGPL-2.1 [S: https://github.com/namazso/PawnIO.Modules]. Whether `LpcIO` permits writes: it exposes raw port access per Hayate's own notes (writes are possible in principle) [S: docs/sensors.md]; sources do not state a write policy [S: not found].
- FACEIT blocks PawnIO because its signer also signed cheat software; fixed for FanControl with PawnIO 2.1.0 per the README; Vanguard/EAC/BattlEye not confirmed either way [S: https://github.com/Rem0o/FanControl.Releases/discussions/3537, https://github.com/namazso/PawnIO.Setup/issues/1].

## 2. This PC (FOX), read-only findings [O]

| Item | Value |
|---|---|
| Board | ASUSTeK TUF GAMING B850-PRO WIFI7 W NEO, Rev 1.xx; BIOS AMI 0367 (2026-01-08) |
| ASUS software | Armoury Crate Service 6.7.6.0, ROG FAN XPERT 4 (4.02.09), ASUS AIOFan HAL 1.5.10.0, ASUS Framework, AURA components |
| ASUS services running | ArmouryCrateService, AsusFanControlService (2.05.06), asComSvc (AXSP), AsusCertService |
| Fan service dir | AsusFanControlService\2.05.06 includes ASUS_WMI.dll, ACPIWMI.dll, cpuidsdk64.dll, Fan.proto, FanStore.xml |
| Kernel drivers running | Asusgio3 (AsIO3.sys), PawnIO.sys, WinRing0x64.sys, cpuz159, SONiXDDR_ASUS |
| root\wmi ASUS classes | ASUSManagement (has GetFanPolicy, SetFanPolicy, GetManualFanCurve, SetManualFanCurve, GetManualFanCurvePro, SetManualFanCurvePro, read/write_smbus_*, asio_hw_fun*), AsusLedControlWmi, AsusWpbtWmi, AsusMiniZipWmi, AsusWifiOptionWmi. No ASUSHW class. |
| USB AIO vendors | None. No Corsair 1B1C, NZXT 1E71, Asetek 2433. ASUS 0B05 devices present: 1CD2 (AURA LED Controller), 1BA4 (ROG Gaming Display Aura), 1AD3 (headset), hub 0BA6/1BA6 |
| Other devices | Razer keyboard/mouse, Shure MV7, FiiO DAC, Stream Deck-class 0FD9:006C |

Meaning:
- AIO model is not identifiable. A USB-controlled AIO (Corsair, NZXT, ASUS Ryujin) would normally enumerate; none does, and `ASUS AIOFan HAL` is only an installed Armoury Crate component, not proof of an ASUS AIO [I].
- Most likely: AIO radiator fans on CPU_FAN, pump on AIO_PUMP, plain 4-pin PWM, RGB via the AURA controller (1CD2) [I]. Then there is no AIO protocol to speak; the only control path is the motherboard header.
- The ASUS WMI interface on this board is `ASUSManagement` with fan-curve methods, not the older `ASUSHW` that FanControl.AsusWMI documents (v3) [O + S]. Existing plugin support for this board is therefore unconfirmed [I]. I did not invoke any method.
- Fans are already owned by AsusFanControlService/Armoury Crate [O].
- Three kernel drivers are already loaded (WinRing0 included, which Hayate's Defender stance wants to avoid) [O].

## 3. Options

### (C) Display-only (recommended first)
Design: add rows to the existing Vitals panels.
- GPU fan RPM: already done (D3DKMT).
- CPU fan / AIO pump / chassis RPM and board temps: need SuperIO read via `LpcIO`, or ASUS WMI read, or ASUS EC (`LpcACPIEC`/`IsaBridgeEC`). Hayate dropped ITE for lack of validation [S: docs/sensors.md]; NCT6799-style 16-bit fan counter decode and chip detection are required [I].
- Alternative with zero driver work: show nothing new, only relabel what exists.
Effort: 1-2 weeks incl. validation on this board (rig has a different board; needs FOX or a read-only live test [I]).
Risk: low but not zero. Config-mode entry/exit on LPC can disturb ASUS EC if it races; use the same `Access_ISABUS` named mutant plus short read windows [S: mutex noted in LHM context; exact name per brief; unverified here].
Admin: PawnIO open already needs elevation [S: docs/sensors.md]. No new UAC beyond existing "Restart as admin".
Driver: reuses PawnIO; no new driver; no writes.

### (B) FanControl integration
Design: Hayate detects FanControl.exe, shows its tray status, maybe launches it with a saved config.
- No documented CLI or API for profile switching [S: README]. Would have to launch a second instance with a config path, or edit its profile files and restart it [I]. Fragile, version-bound.
- FanControl still must run elevated to write; Hayate adds nothing to the safety story.
- On this board FanControl needs the AsusWMI plugin or SuperIO control, which must coexist with Armoury Crate [S, O].
Effort: 3-5 days to launch/detect only; more if it reads FanControl's data [I].
Risk: low for Hayate; the risk stays inside FanControl.
Admin: FanControl runs as its own service/elevated app; Hayate needs none.
Sacrifices: relies on closed-source tool with undocumented control surface.

### (A) Native control in Hayate
Design:
1. Privileged helper (service or elevated child) holding PawnIO write handle; GUI sends presets over a named pipe.
2. SuperIO driver written in Rust over `LpcIO` for the board chip (Nuvoton NCT67xx family): chip ID, bank select, PWM/fan-mode registers, per-header manual/SmartFan switch, restore on exit.
3. Curve engine with hysteresis and step limits, sensors from existing sampler.
4. Watchdog: helper restores BIOS/auto mode on exit; a second guard needed for crashes [I].
5. Pump: only a PWM duty on AIO_PUMP if it is a motherboard header; no HID protocol needed on this PC [I].
Effort: 4-8 weeks plus on-hardware validation on FOX (cannot be validated on rig) [I].
Risk: high. Bad write can freeze fans; crash leaves manual PWM at last value; ASUS EC/BIOS SmartFan rewrites the same registers; sleep/resume resets state [I].
Admin/driver: elevation always; PawnIO write module usage; possible anti-cheat blocks (FACEIT) [S].
Sacrifices: the defender/dropper posture ("client of PawnIO, never install") and the hidden-cost rule (see 4).

## 4. Performance and risk cost

Existing baseline [S: docs/sensors.md]: CPU temperature is read in a single open/close PawnIO call, no handle held while hidden; GPU/drive reuse the sampler; fan/power rings refresh once per second only while Vitals is painted; no always-on drive polling.

| Item | (C) display | (A) native control |
|---|---|---|
| Hidden cost | ~0 (no reads when hidden) | Not ~0: a loop must run while Hayate is hidden |
| Wakeups | 1/s only while visible [S] | 0.5-1/s continuous: SuperIO read + curve + write [I] |
| CPU time per cycle | tens of microseconds for port IO; PawnIO call overhead dominates [I] | same plus HID/WMI if used; low, but wakeups prevent deep idle [I] |
| LPC bus | brief, mutexed | repeated writes; shared bus with BIOS/EC and other tools (HWiNFO, LHM, AsusFanControlService) [I] |
| USB HID | n/a here | AIO pumps push status ~1/s anyway [I]; reading adds negligible CPU; writes only on change |
| Privilege | existing | permanent elevated daemon [I] |

Key points:
- Curves only work while something runs. Hayate hidden with the window closed would have to keep the sampler plus a control loop alive, or fans stay at a fixed value (dangerous if set low) [I].
- Design option: have a separate tiny helper (not the GUI) own the loop. That keeps UI cost ~0, but turns Hayate into a resident privileged service, which contradicts "costs ~0 when hidden" at the system level [I].
- Failure modes [I]: Hayate crash leaves PWM in manual; sleep/resume may reset or desync; AIO re-enumeration after resume loses HID handle; two writers (Armoury Crate, AsusFanControlService) fight [S].
- Defender/AV [S: docs/defender.md row 23, 24]: Hayate is currently a PawnIO client rated Low; adding register writes, a resident elevated service, and possibly HID output reports raises heuristics; row 24-style elevated spawn is already Low-Medium. A resident service with fan writes would likely move the rating up one step [I].
- Anti-cheat: FACEIT blocks PawnIO's signer [S]; others unconfirmed. A resident driver client at game time is a visible risk for a gaming PC [I].
- What it sacrifices: no-driver-install stance, ~0 hidden cost, simple security story, small footprint. I think this is too much for the benefit, because FanControl and Armoury Crate already do it.

## 5. Recommendation and plan

Recommendation: display-only now, no native control, optional FanControl launch helper later.

Phases:
1. (C0, 0.5 day) Verify read-only fan/pump RPM from ASUS WMI/ASUS EC on FOX with a throwaway PowerShell/Rust probe, no writes. Output: which channels exist (CPU_FAN, AIO_PUMP, chassis). [I]
2. (C1, 1-2 weeks) If the SuperIO route is chosen: NCT chip detect and fan-RPM read via `LpcIO`, mutex-protected, only while Vitals is painted; tests on FOX. Else read through ASUS WMI/EC only if a documented read method exists on `ASUSManagement` (names only seen; semantics not verified) [I].
3. (B, 3-5 days, optional) Detect FanControl and add "open FanControl" / show running state. Only after confirming how it behaves with Armoury Crate on FOX.
4. (A, defer) Revisit only if the user retires Armoury Crate and AsusFanControlService and accepts a resident helper. Needs a separate design doc.

Hardware-specific blockers for this PC:
- ASUS B850 TUF board with Armoury Crate + AsusFanControlService + AsIO3 already controlling fans [O].
- `ASUSManagement` WMI exposes SetManualFanCurve but its calling convention is not documented in the sources found; FanControl.AsusWMI documents ASUSHW v3, not this class [O + S].
- AIO model unknown, no USB AIO present, so liquidctl-style protocols do not apply; pump is likely a header [O + I].
- GPU fan control on RTX 5070 Ti via NVML needs admin and Windows/50-series support is unconfirmed [S].
- WinRing0 is currently loaded on FOX by some other software, which will keep Defender/blocklist noise unrelated to Hayate [O + S].

Open uncertainties: exact SuperIO chip on this board (not probed; no port IO done), whether the AIO is ASUS, whether PawnIO `LpcIO` accepts the writes needed, whether Armoury Crate locks the registers, Vanguard/EAC/BattlEye behaviour with PawnIO.
