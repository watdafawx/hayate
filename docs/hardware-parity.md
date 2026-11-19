# Vitals hardware parity with Task Manager

What Task Manager's Performance tab lists next to its graphs, and where each
field comes from in Hayate. Scope: CPU, memory, GPU, disk. This is not a claim of
parity with all of Task Manager (see "Not covered").

Layout: a footer inside each device's graph panel, below the graph and above the
lower sections (`src/ui/hwfacts.rs`). Live readings on the left, hardware facts on
the right; narrow windows stack them. A value that cannot be read shows `—` or
`Unavailable`, never a zero or a guess.

Static facts are read once (sampler start, GPU load, disk enumeration) and cached
in `sys::Static`, `gpu::Gpu.facts`, `VDisk`. The one live extra, the CPU clock,
is polled on the I/O worker only while the CPU detail is open. All parsing is in
`src/sample/hw.rs` with unit tests; firmware / registry input is bounds checked,
range checked, stripped of control characters and capped.

## CPU

| Field | Source | Notes |
|---|---|---|
| Utilization, processes, threads, handles, up time | existing sampler | unchanged |
| Speed (current) | PDH `\Processor Information(*)\% Processor Performance` x `Processor Frequency`, per logical processor, averaged | Estimate from Windows performance counters. Needs two PDH collects: `—` until the second. Goes above base while boosting. Never falls back to base speed. |
| Base speed | `CallNtPowerInformation(ProcessorInformation).MaxMhz` per logical processor; fallback registry `~MHz` | Maximum specified clock reported by Windows (shown as base speed); non-boost equivalence is not guaranteed on every CPU. A range (`2.10–3.40 GHz`) when cores differ (hybrid). Rig: 3.70 GHz = `Win32_Processor.MaxClockSpeed` (the registry `~MHz` reads 3693, so it is only the fallback). |
| Sockets | `GetLogicalProcessorInformationEx` package records | |
| Cores, logical processors | core records; logical = distinct group/bit pairs named by them | Multi-group safe. Hybrid shows `24 (8 P + 16 E)` from efficiency classes. |
| L1 / L2 / L3 (L4 if present) | cache records, deduplicated by (level, type, affinity mask) | Shared caches counted once. L1 is data + instruction summed, like Task Manager. |
| NUMA nodes, processor groups | same call | Shown only when more than one. |
| Virtualization | `IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)` | Firmware flag only. |
| Temperature | existing PawnIO sensor | `—` without admin, as before. |

## Memory

| Field | Source | Notes |
|---|---|---|
| In use, available, committed, cached, paged / non-paged pool | existing sampler | moved from the header / composition meters into the footer; header keeps In use, Available, Committed, Cached, Compressed |
| In use (compressed) | existing Memory Compression working set | tile only when read |
| Speed | SMBIOS type 17 configured speed (offset 0x20, extended 0x58) | Treated as **MT/s**: SMBIOS 3.x defines the field as MT/s (it was MHz in 2.x for DDR). Falls back to rated speed, labelled `Speed (rated)`. Mixed modules show a range. A rated speed is added when the board runs below it. |
| Slots used | count of type 17 records, populated / total | |
| Form factor, type | type 17 form factor / memory type | `DIMM / SODIMM` when mixed |
| Installed, hardware reserved | `GetPhysicallyInstalledSystemMemory` minus usable physical memory | Shown only when installed >= usable. Rig: 100 MB, same as Task Manager. |
| Per module | locator (bank-qualified when locators repeat per channel), size, type, MT/s, maker, part | One line each (first 8), so mixed kits stay visible as mixed. Placeholder strings ("Unknown", "To Be Filled...") are dropped. |

Size field: 15-bit MB, bit 15 = KB units, 0x7FFF = extended size at 0x1C, 0xFFFF =
unknown (treated as empty, never as 64 GB). Records shorter than the field being
read are accepted down to the SMBIOS 2.3 length (27 bytes).

## GPU

| Field | Source | Notes |
|---|---|---|
| Utilization, dedicated / shared usage and capacity | existing D3DKMT sampler | |
| Model | adapter description string, full | |
| Driver version, date | registry display class key (`{4d36e968-...}`), matched by `ven_xxxx&dev_xxxx` from `D3DKMTQueryAdapterInfo(PHYSICALADAPTERDEVICEIDS)`; UMD version as fallback for the version | Two class entries for one PCI id with different versions -> nothing shown rather than a pick. Rig matches `Win32_VideoController` and `Get-PnpDeviceProperty`. |
| Driver model | `D3DKMT DRIVERVERSION` | `WDDM 3.2` |
| PCI ID | physical adapter ids | |
| Physical location | `D3DKMT ADAPTERADDRESS` | `PCI bus 6, device 0, function 0`; rig matches `DEVPKEY_Device_LocationInfo`. |

## Disk

| Field | Source | Notes |
|---|---|---|
| Active time, read / write rates, average response | existing disk counters | `Response` shows `—` when no I/O completed (it used to show 0 ms). |
| Model, firmware | storage descriptor (`ProductId`, `ProductRevision`) | |
| Type | bus type + seek-penalty descriptor | `SSD · NVMe`, `HDD · SATA`; `Unknown` when the drive does not say (NVMe without the flag counts as SSD). |
| Capacity, mapped volumes | disk length; sum of mapped drive-letter volume sizes | Decimal units (GB = 1e9), unlike Task Manager's GiB: a "250 GB" drive shows 250 GB here, 232 GB there. |
| System disk, page file | existing | |

## Not covered

| Item | Why |
|---|---|
| GPU hardware-reserved memory | No documented query; the value Task Manager shows is not derivable from the D3DKMT segment data without guessing. Left out. |
| DirectX feature level | Needs a D3D device per adapter (loads the driver). Not worth a startup cost or a failure mode. |
| VM / hypervisor detection | The CPUID hypervisor bit is set under VBS as well; no reliable "is a VM" signal. |
| Per-core graphs past 64 logical processors | The load graph sizes from `GetSystemInfo` (group 0). Topology counts are right on multi-group systems; the heatmap is not. |
| CPU current speed per core, uptime breakdown, disk queue per volume, NIC details | out of this batch |
| GiB display option | decimal only for now |

## Verification

Rig (Ryzen 9 5900X, 4x16 GB DDR4-3600, RTX 3080, four disks) against
`Win32_Processor`, `Win32_PhysicalMemory`, `Win32_VideoController`,
`Get-PhysicalDisk`, `Get-Volume`, `Get-Counter` and `Get-PnpDeviceProperty`:
12 cores / 24 logical / 1 socket, L1 768 KB, L2 6 MB, L3 64 MB, virtualization on,
base 3.70 GHz, live speed within the counters' range, 3600 MT/s, 4 of 4 slots,
DIMM / DDR4, 64 GB installed, 100 MB reserved, GPU driver 32.0.16.1714 dated
2026-09-17 at PCI bus 6, disk model / firmware / SSD NVMe / 250 GB / system disk /
page file. These are the rig's values, not those of any other machine.

Integration review: PDH uses NOCAP100 so boost readings exceed 100%; returning after >4 s inactivity rebaselines before showing speed. Unknown-capacity DIMMs retain occupied-slot status. Partial speed coverage is disclosed. Mapped volumes is not physical formatted capacity: mounted-folder volumes and multi-extent mappings remain incomplete. Firmware topology masks are bounded by each record.
