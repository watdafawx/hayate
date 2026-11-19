"""Builds the Hayate brand SVGs + PNGs. Needs: fonttools, ImageMagick (magick).
    python assets/brand/build.py
Text is converted to outlines (Barlow Semi Condensed for Latin, Noto Sans JP Bold for kanji; both SIL OFL, in fonts/) so SVGs render anywhere."""
import os, subprocess, math
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen

HERE = os.path.dirname(os.path.abspath(__file__))
ACC, BG, PAN, LN, TX1, TX3 = "#56c8d8", "#0f1217", "#151a21", "#29303b", "#e8ebf0", "#8a93a1"
K = math.tan(math.radians(13))  # italic shear

def poly(pts): return "M" + " L".join(f"{x:.1f} {y:.1f}" for x, y in pts) + "Z"

def sheared(x0, x1, y0, y1, cy=256):
    return [(x0 + (cy - y0) * K, y0), (x1 + (cy - y0) * K, y0), (x1 + (cy - y1) * K, y1), (x0 + (cy - y1) * K, y1)]

def glyph(small=False):
    """Italic H: two leaning stems joined by one rising blade (the crossbar)."""
    sw = 88 if small else 64           # stem width
    top, bot = (88, 424) if small else (100, 412)
    lx, rx = (112 if small else 142), (312 if small else 306)
    p = [sheared(lx, lx + sw, top, bot), sheared(rx, rx + sw, top, bot)]
    t = 80 if small else 56            # blade thickness
    yl, yr = (308 if small else 296), (212 if small else 196)   # blade bottom edge at left / right stem
    # blade runs between the stems (overlapping a little so it fuses); ends follow the stem lean
    def xs(x, y): return x + (256 - y) * K
    xl, xr = lx + sw - 6, rx + 6
    bar = [(xs(xl, yl), yl), (xs(xr, yr), yr), (xs(xr, yr + t), yr + t), (xs(xl, yl + t), yl + t)]
    bar = [(xs(xl, yl - t), yl - t), (xs(xr, yr - t), yr - t), (xs(xr, yr), yr), (xs(xl, yl), yl)]
    return " ".join(poly(q) for q in p) + " " + poly(bar)

def tile(fill=PAN, ring=LN, r=116, small=False):
    if small:
        return f'<rect width="512" height="512" rx="96" fill="{fill}"/>'
    return f'<rect width="512" height="512" rx="{r}" fill="{fill}"/><rect x="4" y="4" width="504" height="504" rx="{r-4}" fill="none" stroke="{ring}" stroke-width="8"/>'

def svg(w, h, inner, vb=None, extra=""):
    vb = vb or f"0 0 {w} {h}"
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="{vb}" {extra}>{inner}</svg>\n'

def mark(small=False, tile_on=True, color=ACC):
    return (tile(small=small) if tile_on else "") + f'<path d="{glyph(small)}" fill="{color}"/>'

_fonts = {}
def font(name, wght=None):
    """Load a font from assets/brand/fonts; pin a variable font to one weight."""
    key = (name, wght)
    if key not in _fonts:
        f = TTFont(os.path.join(HERE, "fonts", name))
        _fonts[key] = instancer.instantiateVariableFont(f, {"wght": wght}) if wght else f
    return _fonts[key]

def text_path(txt, fnt, size, x, y, track=0):
    gs, cmap, upm = fnt.getGlyphSet(), fnt.getBestCmap(), fnt["head"].unitsPerEm
    s, cur, out = size / upm, x, []
    for ch in txt:
        g = cmap[ord(ch)]
        pen = SVGPathPen(gs)
        gs[g].draw(TransformPen(pen, (s, 0, 0, -s, cur, y)))
        out.append(pen.getCommands())
        cur += gs[g].width * s + track
    return " ".join(out), cur

def write(name, s):
    open(os.path.join(HERE, name), "w", encoding="utf-8").write(s)

def png(src, dst, w, h=None):
    cmd = ["magick", "-background", "none", "-density", "384", os.path.join(HERE, src), "-resize", f"{w}x{h or w}", os.path.join(HERE, dst)]
    subprocess.run(cmd, check=True)

def main():
    latin = font("BarlowSemiCondensed-Medium.ttf")
    cjk = font("NotoSansJP-Bold-subset.ttf")
    write("hayate-mark.svg", svg(512, 512, mark(), extra='role="img" aria-label="Hayate"'))
    write("hayate-mark-small.svg", svg(512, 512, mark(small=True), extra='role="img" aria-label="Hayate"'))
    write("hayate-mark-mono.svg", svg(512, 512, f'<path d="{glyph()}" fill="currentColor"/>', extra='color="#000"'))

    # wordmark: mark 120px + Hayate + 疾風
    H = 160
    d1, e1 = text_path("Hayate", latin, 84, 190, 96, track=2)
    d2, e2 = text_path("疾風", cjk, 30, 192, 136, track=6)
    W = int(max(e1, e2) + 24)
    inner = (f'<g transform="translate(16 20) scale({120/512})">{mark()}</g>'
             f'<path d="{d1}" fill="{TX1}"/><path d="{d2}" fill="{TX3}"/>')
    write("hayate-wordmark.svg", svg(W, H, inner))

    # banner 1280x320
    tag = "Swift, light, in charge of your PC."
    dm, _ = text_path("Hayate", latin, 120, 400, 170, track=3)
    dj, ej = text_path("疾風", cjk, 40, 404, 218, track=8)
    dt, _ = text_path(tag, latin, 32, 404, 268)
    streaks = "".join(f'<rect x="{x}" y="{y}" width="{w}" height="2" fill="{LN}"/>' for x, y, w in [(760, 90, 420), (880, 60, 300), (700, 250, 480), (960, 280, 220)])
    b = (f'<rect width="1280" height="320" fill="{BG}"/>' + streaks.replace('<rect', '<rect opacity=".0"') * 0 +
         f'<g transform="translate(110 56) scale({208/512})">{mark()}</g>'
         f'<path d="{dm}" fill="{TX1}"/><path d="{dj}" fill="{ACC}"/><path d="{dt}" fill="{TX3}"/>'
         f'<rect x="0" y="316" width="1280" height="4" fill="{ACC}" opacity=".9"/>')
    write("banner.svg", svg(1280, 320, b))

    # social 1200x630
    dm2, _ = text_path("Hayate", latin, 150, 0, 0, track=4)
    dj2, _ = text_path("疾風", cjk, 52, 0, 0, track=10)
    dt2, _ = text_path(tag, latin, 38, 0, 0)
    s = (f'<rect width="1200" height="630" fill="{BG}"/>'
         f'<g transform="translate(100 150) scale({330/512})">{mark()}</g>'
         f'<g transform="translate(500 330)"><path d="{dm2}" fill="{TX1}"/></g>'
         f'<g transform="translate(506 410)"><path d="{dj2}" fill="{ACC}"/></g>'
         f'<g transform="translate(506 480)"><path d="{dt2}" fill="{TX3}"/></g>'
         f'<rect x="0" y="624" width="1200" height="6" fill="{ACC}"/>')
    write("social.svg", svg(1200, 630, s))

    png("banner.svg", "banner.png", 1280, 320)
    png("social.svg", "social.png", 1200, 630)
    os.makedirs(os.path.join(HERE, "png"), exist_ok=True)
    for n in (16, 24, 32, 48, 64, 128, 256, 512):
        png("hayate-mark-small.svg" if n <= 32 else "hayate-mark.svg", f"png/hayate-{n}.png", n)

if __name__ == "__main__":
    main()
