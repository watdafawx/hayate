# Hayate brand (疾風, "swift wind")

- **Mark:** an italic **H** of two leaning stems joined by one rising blade; the lean is speed, the blade is the wind-stroke, the H is the name. Three polygons, no curves, no text; reads at 16 px (`hayate-mark-small.svg` is the heavier, simplified 16/24/32 cut).
- **Colors:** accent `#56c8d8` (the app's `ACC`), tile `#151a21` (`PAN`) with `#29303b` ring (`LN`), page `#0f1217` (`BG`), wordmark `#e8ebf0` (`TX1`), meta `#8a93a1` (`TX3`). One accent only; mono version is `currentColor`.
- **Clear space:** keep a margin of one stem width (about 1/8 of the mark's height) on all sides; never place other graphics inside it.
- **Min size:** 16 px (use the small cut below 48 px); the wordmark's 疾風 line needs at least 24 px of cap height, otherwise drop it and use "Hayate" alone.
- **Rebuild:** `python assets/brand/build.py` (needs `fonttools` and ImageMagick); text is outlined (Barlow Semi Condensed, Noto Sans JP; both SIL OFL 1.1, bundled in `assets/brand/fonts/`) so SVGs need no font at runtime and the build needs no system fonts. `res/blink.ico` is built from `assets/brand/png/` (16-256).
