# Render path: GPU (hardware) vs software (WARP)

Blink draws with Direct2D. Since this change it renders on the GPU by default and falls back to software (WARP) on its own. `BLINK_GPURENDER=0` forces software.

## How it works (`src/gfx.rs`)
- **Open:** the first frame of every open is painted with a software target (~15–20 ms), so an open never waits for the GPU driver. Once that frame is up, a worker thread creates the D3D11 device (150–220 ms on an RTX 3080, mostly driver load). The next paint then swaps in a hardware target (31–41 ms, because the driver is loaded by then) and drops the warm device.
- **Hide:** the target, its device and the D2D factory are released, so the NVIDIA user-mode driver (`nvwgf2umx.dll`) or `d3d10warp.dll` unloads. Hidden Blink holds no D3D device and no VRAM.
- **Device loss** (`D2DERR_RECREATE_TARGET`: TDR, driver update or reset, adapter removed): the target and the factory are dropped and the next paint uses software. The GPU is tried again on the first new target after `GPU_RETRY` (5 min). A GPU target that fails to create, or a warm-up device that fails, takes the same fallback.
- **Testing:** debug builds only: `BLINK_TEST_DEVICELOST=<n>` reports frame n as a lost device.

## Measurements (2026-11-16)
Test machine: Ryzen 9 5900X (24 threads), RTX 3080, 4K at 200%. Release build. Window 2214x1427 px. `BLINK_TIMING=1` `paintstats:` lines (30 s windows), process counters sampled over 8–20 s. Load = 18 BelowNormal busy threads (6 of 24 free; total CPU 90–95%).

| | Software (WARP) | GPU (default) |
|---|---|---|
| Private bytes, shown 60 s+ (Vitals / Processes) | 59–64 / 59–61 MB | 83–84 / 85 MB |
| Working set, shown (Vitals / Processes) | 67–84 / 80–84 MB | 53–59 / 52–53 MB |
| Blink's own Memory column for itself | 45 MB | 12 MB |
| VRAM (dedicated / shared) | 0 / 12 MB | 37 / 3 MB |
| Launch: first paint / real data shown | 85 / 206 ms | 79 / 213 ms |
| Open from tray: first paint | 30–32 ms | 27–41 ms (software frame), GPU from ~200–260 ms |
| Paint avg / p95, Vitals | 8.1–8.9 / 9.4–10.8 ms | 3.0–3.8 / 3.5–4.9 ms |
| Paint avg / p95, Processes, idle | 7.4–8.0 / 9.6–10.2 ms | 3.6–5.1 / 5.0–6.6 ms |
| Paint avg / p95 / max, Processes, under load | 8.7–10.7 / 10.0–20.5 / 48 ms | 5.4–5.6 / 6.4–6.5 / 6.8 ms |
| EndDraw (rasterise + present) avg | 3.0–4.2 ms | 0.5–0.8 ms |
| Process CPU, % of one core (idle / load) | 2.9–4.6 / 3.8–4.6 | 1.7–2.2 / 2.1–2.4 |
| Hidden after open: private / working set | 18–20 / 5–6 MB | 21–23 / 6–10 MB |
| Hidden: D3D device, driver DLL, VRAM | none, WARP unloaded, 0 | none, NVIDIA UMD unloaded, 0 |
| GPU engine use while shown | ~0.2% (WARP presents through DWM) | 0.05–0.4% (Blink's GPU column shows 0.0%) |
| Device loss | n/a | software target in 3.9 ms, GPU retried after 5 min |

## Trade-off
- **GPU gains:** paints 2–3x faster, EndDraw 5x faster, p95 under CPU load 3x lower (no 20–50 ms frames), about half the CPU, and 25–30 MB less working set (the number Task Manager shows).
- **GPU costs:** about 24 MB more private commit (driver heaps) and 37 MB of VRAM while shown; one extra ~40 ms paint about 0.2 s after each open (the swap to the GPU target); dependence on the GPU driver, which the software fallback covers. Hidden cost is the same as software (within ~3 MB).
