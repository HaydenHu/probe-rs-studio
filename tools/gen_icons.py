"""Generate the vector icon assets used by ProbeRS Studio.

Pure standard library. One geometry definition per icon is emitted twice:

* ``assets/icons/*.svg`` -- the real vector source, parsed and drawn with
  epaint paths at runtime (resolution independent, no textures).
* ``assets/icons/app-icon.rgba`` -- a 64x64 raw RGBA raster of the brand mark
  on the accent-blue plate. Windows taskbar/title-bar icons must be bitmaps;
  this is the only raster in the project.

Run:  python tools/gen_icons.py
"""

import math
import os
import struct

N = 64          # viewBox size
S = 4           # supersampling for the OS icon raster only
H = N * S
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "assets", "icons")


def clamp(v, lo=0.0, hi=1.0):
    return lo if v < lo else (hi if v > hi else v)


# --------------------------------------------------------------------------
# Geometry: every icon is a list of primitives in a 64x64 viewBox.
#   ('rect',   cx, cy, w, h, r, width, filled)
#   ('line',   x1, y1, x2, y2, width)
#   ('poly',   [(x, y), ...], width, filled)
#   ('circle', cx, cy, r, width, filled)
# --------------------------------------------------------------------------

def r(cx, cy, w, h, radius=5.0, width=3.4, filled=False):
    return ("rect", cx, cy, w, h, radius, width, filled)


def l(x1, y1, x2, y2, width=3.4):
    return ("line", x1, y1, x2, y2, width)


def poly(points, width=3.4, filled=False):
    return ("poly", points, width, filled)


def c(cx, cy, radius, width=3.4, filled=False):
    return ("circle", cx, cy, radius, width, filled)


def gear_tooth(k):
    ang = k * math.pi / 4
    ca, sa = math.cos(ang), math.sin(ang)
    pts = []
    for dr, dt in ((-4.5, -3.5), (4.5, -3.5), (4.5, 3.5), (-4.5, 3.5)):
        rr = 17 + dr
        pts.append((32 + rr * ca - dt * sa, 32 + rr * sa + dt * ca))
    return poly(pts, filled=True)


BOLT = [(35, 19), (26, 34), (31, 34), (28, 45), (39, 29), (33, 29), (37, 19)]


def geometry():
    workbench = [
        r(32, 32, 44, 40, 6),
        l(30, 12, 30, 52, 3.0),
        l(30, 32, 54, 32, 3.0),
    ]
    serial = [
        r(32, 29, 40, 26, 5),
        c(22, 29, 2.7, filled=True),
        c(32, 29, 2.7, 2.2),
        c(42, 29, 2.7, 2.2),
        l(32, 42, 32, 54, 3.2),
    ]
    chips = [r(32, 32, 24, 24, 3)]
    for y in (25, 32, 39):
        chips += [l(11, y, 20, y, 3.0), l(44, y, 53, y, 3.0)]
    for x in (25, 32, 39):
        chips += [l(x, 11, x, 20, 3.0), l(x, 44, x, 53, 3.0)]
    chips.append(c(26, 26, 2.0, filled=True))
    logs = [
        r(32, 32, 44, 36, 5),
        l(10, 24, 54, 24, 2.8),
        poly([(19, 32), (24, 36), (19, 40)], 2.8),
        l(29, 36, 44, 36, 2.8),
        l(29, 43, 39, 43, 2.8),
    ]
    settings = [c(32, 32, 14, 4.0), c(32, 32, 6, 3.0)]
    settings += [gear_tooth(k) for k in range(8)]
    logo = [r(32, 32, 36, 36, 7)]
    for y in (24, 40):
        logo += [l(8, y, 14, y, 3.0), l(50, y, 56, y, 3.0)]
    for x in (24, 40):
        logo += [l(x, 8, x, 14, 3.0), l(x, 50, x, 56, 3.0)]
    logo.append(poly(BOLT, filled=True))
    return {
        "workbench": workbench,
        "serial": serial,
        "chips": chips,
        "logs": logs,
        "settings": settings,
        "logo": logo,
    }


# --------------------------------------------------------------------------
# SVG emitter (the vector source consumed by src/icons.rs)
# --------------------------------------------------------------------------

def fmt(v):
    return f"{v:.3f}".rstrip("0").rstrip(".") if isinstance(v, float) else str(v)


def points_attr(points):
    return " ".join(f"{fmt(x)},{fmt(y)}" for x, y in points)


def to_svg(prims):
    out = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"',
        '     width="64" height="64" fill="none" stroke="#FFFFFF"',
        '     stroke-width="3.4" stroke-linecap="round" stroke-linejoin="round">',
    ]
    for prim in prims:
        kind = prim[0]
        if kind == "rect":
            _, cx, cy, w, h, rad, width, filled = prim
            attrs = (f'x="{fmt(cx - w / 2)}" y="{fmt(cy - h / 2)}" '
                     f'width="{fmt(w)}" height="{fmt(h)}" rx="{fmt(rad)}"')
            if filled:
                out.append(f'  <rect {attrs} fill="#FFFFFF" stroke="none"/>')
            else:
                out.append(f'  <rect {attrs} stroke-width="{fmt(width)}"/>')
        elif kind == "line":
            _, x1, y1, x2, y2, width = prim
            out.append(f'  <line x1="{fmt(x1)}" y1="{fmt(y1)}" x2="{fmt(x2)}" '
                       f'y2="{fmt(y2)}" stroke-width="{fmt(width)}"/>')
        elif kind == "poly":
            _, points, width, filled = prim
            tag = "polygon" if filled else "polyline"
            extra = ' fill="#FFFFFF" stroke="none"' if filled else f' stroke-width="{fmt(width)}"'
            out.append(f'  <{tag} points="{points_attr(points)}"{extra}/>')
        elif kind == "circle":
            _, cx, cy, radius, width, filled = prim
            attrs = f'cx="{fmt(cx)}" cy="{fmt(cy)}" r="{fmt(radius)}"'
            if filled:
                out.append(f'  <circle {attrs} fill="#FFFFFF" stroke="none"/>')
            else:
                out.append(f'  <circle {attrs} stroke-width="{fmt(width)}"/>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


# --------------------------------------------------------------------------
# Rasteriser, used only for the Windows OS window icon.
# --------------------------------------------------------------------------

def sd_rounded(px, py, cx, cy, w, h, r):
    qx, qy = abs(px - cx) - (w / 2 - r), abs(py - cy) - (h / 2 - r)
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r


def sd_segment(px, py, x1, y1, x2, y2):
    dx, dy = x2 - x1, y2 - y1
    t = clamp(((px - x1) * dx + (py - y1) * dy) / (dx * dx + dy * dy))
    return math.hypot(px - (x1 + t * dx), py - (y1 + t * dy))


def sd_circle(px, py, cx, cy, radius):
    return math.hypot(px - cx, py - cy) - radius


def sd_polygon(px, py, points):
    inside, dmin = False, 1e9
    for i in range(len(points)):
        x1, y1 = points[i]
        x2, y2 = points[(i + 1) % len(points)]
        dmin = min(dmin, sd_segment(px, py, x1, y1, x2, y2))
        if (y1 > py) != (y2 > py):
            xi = x1 + (py - y1) * (x2 - x1) / (y2 - y1)
            if px < xi:
                inside = not inside
    return -dmin if inside else dmin


def primitive_sdf(prim):
    kind = prim[0]
    if kind == "rect":
        _, cx, cy, w, h, rad, width, filled = prim
        return (lambda x, y: sd_rounded(x, y, cx, cy, w, h, rad)), width, filled
    if kind == "line":
        _, x1, y1, x2, y2, width = prim
        return (lambda x, y: sd_segment(x, y, x1, y1, x2, y2)), width, False
    if kind == "poly":
        _, points, width, filled = prim
        return (lambda x, y: sd_polygon(x, y, points)), width, filled
    if kind == "circle":
        _, cx, cy, radius, width, filled = prim
        return (lambda x, y: sd_circle(x, y, cx, cy, radius)), width, filled
    raise ValueError(kind)


def rasterize(prims):
    """Supersampled coverage of the union of all primitives."""
    a = [0.0] * (H * H)
    for prim in prims:
        sdf, width, filled = primitive_sdf(prim)
        for yy in range(H):
            py = (yy + 0.5) / S
            row = yy * H
            for xx in range(H):
                px = (xx + 0.5) / S
                d = sdf(px, py)
                if not filled:
                    d = abs(d) - width / 2.0
                cov = clamp(0.5 - d * S)
                if cov > a[row + xx]:
                    a[row + xx] = cov
    out = [0.0] * (N * N)
    for y in range(N):
        for x in range(N):
            out[y * N + x] = sum(
                a[(y * S + dy) * H + x * S + dx]
                for dy in range(S) for dx in range(S)
            ) / (S * S)
    return out


def make_app_icon_rgba(logo_prims):
    glyph = rasterize(logo_prims)
    plate = rasterize([r(32, 32, 60, 60, 14, filled=True)])
    blue = (0x4D, 0x6B, 0xFE)
    px = bytearray()
    for i in range(N * N):
        a_g = glyph[i]
        px += bytes((
            int(blue[0] * (1 - a_g) + 255 * a_g),
            int(blue[1] * (1 - a_g) + 255 * a_g),
            int(blue[2] * (1 - a_g) + 255 * a_g),
            int(round(plate[i] * 255)),
        ))
    return bytes(px)


def main():
    os.makedirs(OUT, exist_ok=True)
    icons = geometry()
    for name, prims in icons.items():
        path = os.path.join(OUT, name + ".svg")
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(to_svg(prims))
        print(f"  {name + '.svg':<18} {os.path.getsize(path):>6} B  {len(prims)} primitives")
    path = os.path.join(OUT, "app-icon.rgba")
    with open(path, "wb") as f:
        f.write(make_app_icon_rgba(icons["logo"]))
    print(f"  {'app-icon.rgba':<18} {os.path.getsize(path):>6} B  64x64 RGBA (OS window icon)")
    print("done ->", os.path.normpath(OUT))


if __name__ == "__main__":
    main()
