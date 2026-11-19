# Hayate design tokens (v2 palette)

Source of truth for the port of `docs/mock-vitals-lists.html` to Direct2D.
Every value is a solid `0xRRGGBB` hex, ready for the `u32` constants in `src/ui/mod.rs`.
No alpha blending is needed: the tints are precomputed against `PAN`.

## Why the change

- The old signer green `#5ee38a` measures 11.5:1 on the background, almost as bright as titles (`#d8dde5`, 13.7:1), and it is saturated, so it pulled the eye on every Watch row.
- All secondary text used one grey (`DIM #808995`), so there was no hierarchy: everything that was not a title looked the same.
- The background `#0b0d10` was close to pure black, so bright text glared against it.

The fix:

- Four text tiers.
- Semantic colours toned down to sit **below** primary text in luminance, and used only as small badges, dots and 2 px rails, never as long runs of text.
- Surfaces lifted slightly.

## Surfaces

| Token | Hex | Replaces | Where |
|---|---|---|---|
| `BG` | `#0f1217` | `BG #0b0d10` | Window background, body behind panels |
| `PAN` | `#151a21` | `PAN #12151a` | Panels, cards, tables, the Watch feed box |
| `ZEBRA` | `#181d24` | none | Every other row inside a panel (Services, Top lists) |
| `PAN2` | `#1b212a` | `PAN2 #181c23` | Group / hour header bands, hover, Watch unread icon tile, buttons |
| `PAN3` | `#232a35` | none | Active chip / segment, `×N` count badge |
| `LN` | `#29303b` | `LN #232831` | Panel borders (1 px), chip outlines, top border of group bands |
| `SEP` | `#1e242d` | none | Row separators, graph gridlines, header underline inside panels |
| `TRACK` | `#222831` | `BG` used as bar track | Empty part of every bar / meter / thread column |

## Text tiers

| Token | Hex | Use for |
|---|---|---|
| `TX1` | `#e8ebf0` | Names (process, service, app), primary values, unread titles |
| `TX2` | `#b3bac6` | Secondary: read rows, start type, panel titles (uppercase 11 px), legend values, the "what changed" part of a Watch subline |
| `TX3` | `#8a93a1` | Meta: sublines, units, times, publisher, service key name (mono), MB column |
| `TX4` | `#626b78` | Not body text: column headers (10.5 px uppercase), axis ticks, thread numbers, disabled start type. Fails AA by design; never use it for anything people need to read |

Replace `TX` with `TX1`. Replace `DIM` with `TX3` for meta text and with `TX2` where the old DIM text carried meaning (panel titles, read rows).

## Accent and component colours

| Token | Hex | Old | Where |
|---|---|---|---|
| `ACC` = `C_CPU` | `#56c8d8` | `#3fd0e0` | Accent: unread dot, selection rail, toggles on, active rail item; CPU series |
| `C_CPU0` | `#9be3ec` | `#7ee8f2` | CCD0 / V-Cache series and thread bars |
| `C_MEM` | `#a993f0` | `#a78bfa` | Memory. Composition stack: in use `#a993f0`, modified `#6f5fae`, standby `#4a4470`, free `TRACK` |
| `C_GPU` | `#6cc79a` | `#5ee38a` | GPU |
| `C_DSK` | `#e3a857` | `#f5b84a` | Disk read; disk write line `#9a7440` |
| `C_NET` | `#78aaf2` | `#5aa2ff` | Network down; up (mirrored below the axis) `#4f6fa0` |

## Semantic

| Token | Hex | Fill (`*_BG`) | Border (`*_LN`) | Text on fill | Where |
|---|---|---|---|---|---|
| `OK` | `#6cc79a` | `#213232` | `#33574b` | `#a9dcc2` | Running pill, health dots, temp under warm threshold |
| `WARN` | `#e3a857` | `#322e29` | `#5d4c34` | `#ecc890` | Unsigned badge, "Automatic, not running", warn rail (removed / start type changed), commit > 85%, warm temp |
| `HOT` | `#ef7070` | `#34262c` | `#61383d` | n/a | Crash / memory-pressure rail, hot temp, the one red rail badge |
| `PRIV` | `#ad97f2` | `#2a2c3e` | `#4a466a` | n/a | Camera / mic / location rail and kind glyph |
| `ACC_BG` | `#1e323b` | | `#2c5761` | `#a6e0e8` | Selected row fill (plus 2 px `ACC` rail on its left edge); selected tile border |

Rules:

- A semantic colour is **never** used for a run of more than one or two words. Signer names are plain text (`TX3` Microsoft, `TX2` other vendors, both with a 10 px shield glyph). Only **Unsigned** gets a filled badge (`WARN_BG` fill, `WARN_LN` 1 px border, `#ecc890` text).
- Severity on Watch rows is a 2 px rail on the row's left edge (inset 8 px top and bottom), not coloured text. Only warn / hot / privacy kinds get one. New-app and info rows have none.
- The red rail badge (unread count) stays `#d9534f` with white text.

## Contrast (WCAG 2.x)

| Text | BG | PAN | ZEBRA | PAN2 | PAN3 |
|---|---|---|---|---|---|
| TX1 `#e8ebf0` | 15.70 | 14.62 | 14.17 | 13.54 | 12.09 |
| TX2 `#b3bac6` | 9.61 | 8.95 | 8.67 | 8.29 | 7.40 |
| TX3 `#8a93a1` | 6.05 | 5.63 | 5.46 | 5.22 | 4.66 |
| TX4 `#626b78` (non-body) | 3.48 | 3.24 | 3.14 | 3.00 | 2.68 |
| ACC `#56c8d8` | 9.51 | 8.86 | | 8.21 | 7.32 |
| OK `#6cc79a` | 9.18 | 8.55 | | 7.92 | 7.07 |
| WARN `#e3a857` | 8.94 | 8.33 | | 7.71 | 6.88 |
| HOT `#ef7070` | 6.45 | 6.01 | | 5.57 | 4.97 |
| Old DIM `#808995` | 5.30 | 4.94 | | 4.57 | 4.08 |
| Old signer `#5ee38a` | 11.46 | 10.68 | | 9.89 | 8.82 |

- TX1, TX2 and TX3 pass AA (4.5) on every surface. TX1 and TX2 pass AAA (7).
- Pill text: `#a9dcc2` on `OK_BG` 8.75, `#ecc890` on `WARN_BG` 8.49, `#a6e0e8` on `ACC_BG` 9.18.
- Exception: TX3 on `ACC_BG` (a selected row) is 4.30. In a selected row, draw meta text in `TX2`.

## Type

- Text: Segoe UI Variable Text. Numbers, PIDs, times and service keys: Cascadia Mono.
- Body 12.5 px. Row names 12–13 px; unread Watch names are semibold, read ones regular `TX2`.
- Sublines 11.5 px `TX3`.
- Panel titles 11 px semibold uppercase `TX2` with +0.06em tracking. Column headers 10.5 px uppercase `TX4`.
- Big values 24 px mono semibold (tiles 21 px), with the unit at 12 px sans `TX3`.

## Metrics

- Panel radius 7. Pills 10 (height 19). Chips 12 (height 24). Badges 4. Icon tiles 7 (30 px), holding a 20 px app icon.
- Row heights:
  - Services: 27
  - Top lists: 22
  - Watch: 46
  - Group / hour band: 28–30
  - Panel header: 32
- Icons are 16 px in lists and 20 px in Watch tiles. A Watch tile shows the kind glyph in a 15 px circle (`PAN` fill, 1 px `LN` border) at its bottom-right corner, overlapping the tile by 4 px.
- Thread bars: 3 px gap, radius 2, `TRACK` behind, `C_CPU0` for CCD0 and `C_CPU` for CCD1. Each CCD gets a 92 px label column (name, role, average %).
- Graphs: 1 px `SEP` gridlines (3–4 lines), 1.5 px stroke, area fill = series colour at 14–18% opacity. Port that fill with `fill_a` or `spark_range(fill_a)`; it is the only alpha in the design.

## Where each screen uses what

- **Vitals**:
  - Tiles, panels and top lists: `PAN` on `BG`, top lists zebra-striped with `ZEBRA`.
  - Load bars: `C_CPU` on `TRACK`, 4 px tall.
  - Heatmap (direction B): ramp `TRACK #222831` → `#2a8f9e` (at 55%) → `C_CPU0 #9be3ec` (at 100%), one 11 px row per thread, 8 px gap between CCDs.
- **Services**:
  - Table in a `PAN` panel with `ZEBRA` rows.
  - Group bands `PAN2` with `TX1` semibold label and `TX3` mono count. The attention group's label is `#ecc890` with a `WARN` glyph.
  - Status pill: running = `OK_BG` + `OK` dot; stopped = outline dot `TX4`, text `TX3`; Automatic but stopped = `WARN_BG` + `WARN_LN` border.
- **Watch**:
  - Feed in a `PAN` box, hour bands in `PAN2`, rows separated by `SEP`.
  - Unread = `ACC` dot + `PAN2` icon tile + semibold `TX1` name. Read = transparent tile with `SEP` outline + `TX2` name.
  - Toggles: on = `ACC` track with a `BG` knob; off = `TRACK` with an `LN` ring and a `TX4` knob.
