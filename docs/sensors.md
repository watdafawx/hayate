# Vitals sensor panels

CPU, GPU and per-disk details include Sensors directly below their headers,
following `docs/mock-vitals-sensors.html`. The rail has seven tabs. Each panel
has a 1m / 10m / 1h chart and channel rows with sparklines, range bars,
current values and min–max. Temperature charts mark WARM/HOT thresholds:
CPU 80/90 °C, GPU 80/88, SSD 60/70, HDD 45/55 (1 °C hysteresis).
Warm/hot dots and drive temperatures also appear in the device list.
PawnIO notices and expandable Not detected reasons dock below CPU details.

Local fan, power and drive rings refresh once per second while Vitals is
painted. Time away stays a gap, including gaps inside chart buckets. CPU and
GPU temperature reuse sampler history (`cput`, `gput{i}`); background sampling
and the History tab remain intact. Drive queries retain their existing
15-second visible-window cadence; no always-on drive polling was added.
SATA SSD/HDD classification uses the existing seek-penalty flag.

Debug builds: `HAYATE_TAB=1` opens Vitals (`3` now opens History). Relaunch IDs
retain old numbering: an old Sensors selection restores Vitals; other tab
selections survive updates unchanged.

Sources:

- **CPU** — PawnIO (`AMDFamily17` module), reading the AMD Zen SMN thermal
  register over PCI config space.
- **GPU** — the existing D3DKMT code in `src/sample/gpu.rs` (temperature, fan
  RPM, power %), reused, not duplicated.
- **Storage** — the existing `src/sample/disk.rs` storage-temperature query,
  via the background snapshot.
- **Motherboard / Memory** — not implemented (see below).

## PawnIO integration

Hayate loads PawnIO at runtime with `LoadLibraryW`/`GetProcAddress` and calls
only the documented user-mode API (`pawnio_open`, `pawnio_load`,
`pawnio_execute`, `pawnio_close`). It never installs, starts, or otherwise
touches the PawnIO driver or service. If `PawnIOLib.dll` is missing or the
service is not running, the CPU panel shows **"PawnIO not installed"** and the GPU
and storage rows keep working.

The PawnIO *executor* handle is opened and closed inside a single read, so no
handle is ever held while the window is hidden. The `PawnIOLib.dll` module base
and its four function pointers are resolved once and cached for the process
lifetime (the DLL itself is never unloaded).

To avoid racing other monitoring tools that also read PCI config space (HWiNFO,
LibreHardwareMonitor), each SMN read briefly acquires the named mutant
`\BaseNamedObjects\Access_PCI` (best-effort; the read proceeds without it if
the mutant does not exist or is busy).

## Embedded module

The signed module binary is embedded with `include_bytes!` (no runtime
downloads) from `res/pawnio/`:

| File              | Source                                      | Release tag | SHA-256 (of the .bin)                                  |
|-------------------|---------------------------------------------|-------------|--------------------------------------------------------|
| `AMDFamily17.bin` | `github.com/namazso/PawnIO.Modules/releases`| `0.2.11`    | `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91` |

The release asset is `release_0_2_11.zip`
(SHA-256 `43608cb89bc84247fef1368a139013f7d043e17db6d6c8dfc9b46bf0905a81f4`),
downloaded over HTTPS and verified before extracting the `.bin` above.

## CPU temperature decoding

`src/sensors.rs::decode_tctl` decodes the raw value read from SMN register
`THM_TCON_CUR_TMP` (`0x00059800`) via `ioctl_read_smn`, matching
LibreHardwareMonitor's `Amd17Cpu.cs`:

```text
CUR_TEMP = raw[31:21]        # 0.125 °C steps
RANGE_SEL = raw[19]
TJ_SEL    = raw[17:16]

offset = (RANGE_SEL != 0) || (TJ_SEL == 0b11)
t = (raw >> 21) * 0.125
if offset: t -= 49.0
```

A Ryzen 9 5900X (Zen 3, family 19h) has no additional Tctl offset, so
`Tctl == Tdie`. The result is clamped to a plausible range (`-20..130 °C`) so a
torn read (e.g. a race with another tool) is discarded rather than displayed.

## Not implemented

- **Motherboard (ITE Super I/O)** — the task called for the `IT87XX` PawnIO
  module, but no such module exists in `PawnIO.Modules`. The only Super I/O
  path is the generic `LpcIO` module (raw index/data port access), on top of
  which a full IT87XX driver would have to be written by hand: ITE config-mode
  entry/exit (`0x87/0x01/0x55`), chip-ID detection, logical-device selection,
  the EC base-address register, per-chip temperature/fan register maps and the
  16-bit fan-counter protocol. That is ~200 lines of register-level code that
  cannot be validated in this environment (compile-only; no hardware access),
  and getting the config-mode *exit* wrong can leave the EC in config mode and
  disrupt board fan control. So the CPU panel reports **"board sensors unavailable"**
  until this can be implemented and tested on real hardware.
- **Memory (DIMM / SPD thermal sensor)** — DDR4 SPD thermal sensing (JEDEC
  TSE2004) needs SMBus access (`SmbusPIIX4`/`SmbusI801`) plus SPD device
  scanning, well over the ~150-line budget for an optional feature.

## Register-decoding uncertainty (for a reviewer on real hardware)

- The SMN read depends on the `AMDFamily17` module's `main()` accepting family
  17h–1Ah (the 5900X is family 19h, model 21h — in range).
- The `-49 °C` offset uses both `RANGE_SEL[19]` and `TJ_SEL[17:16]` exactly as
  LibreHardwareMonitor does; this is the part most worth verifying on the
  actual 5900X.
- `ioctl_read_smn` is documented to require the `Access_PCI` mutant; the read
  also checks `(vendor & 0xFFFF) == 0x1022` and returns `STATUS_NOT_SUPPORTED`
  on non-AMD systems, which surfaces as "CPU sensor read failed".

## Install and admin (other machines)

- PawnIO is not bundled. The Vitals CPU panel offers **Install PawnIO** (`winget install -e --id namazso.PawnIO`, UAC prompt) when `PawnIOLib.dll` is not found under `%ProgramFiles%\PawnIO`.
- `pawnio_open` fails with access denied unless the process is elevated. The CPU panel then offers **Restart as admin**: a UAC-prompted relaunch (`--wait-pid <old pid>` so the new copy waits for the old one to exit). Nothing elevates silently and there is no logon task.
- Without PawnIO or admin, GPU and drive temperatures still work.
- Test headless on a machine: `hayate.exe --test-cpu-temp` (debug builds), plain and under `gsudo`. Rig 5900X read 61.2 C elevated.
