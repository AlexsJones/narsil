#!/usr/bin/env python3
"""Render what a terminal shows to a PNG. Needs Pillow, and fontconfig's fc-match.

Reads the output of `tmux capture-pane -e -p` (text with its colours and
styles as escape sequences) and draws it cell by cell, as a terminal would:
the same characters in the same colours. Lines and blocks are drawn as shapes
so that they join up; everything else is set in the terminal's font.

    tmux capture-pane -e -p -t pane | render.py out.png [--bg '#282828' --fg '#ebdbb2']
"""
import argparse, functools, re, subprocess, sys
from PIL import Image, ImageDraw, ImageFont

FAMILY = "Adwaita Mono"
STYLES = {"Regular": "", "Bold": ":bold", "Italic": ":italic", "BoldItalic": ":bold:italic"}

def font_file(style):
    """The terminal's font in a style, or the nearest monospace font there is."""
    return subprocess.run(["fc-match", "--format=%{file}", f"{FAMILY}:spacing=mono{STYLES[style]}"], capture_output=True, text=True).stdout

BASE16 = ["#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
          "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2"]

def rgb(text):
    text = text.lstrip("#")
    return tuple(int(text[i:i + 2], 16) for i in (0, 2, 4))

def indexed(n):
    if n < 16:
        return rgb(BASE16[n])
    if n < 232:
        n -= 16
        return tuple(0 if v == 0 else 55 + 40 * v for v in (n // 36, n // 6 % 6, n % 6))
    return (8 + 10 * (n - 232),) * 3

def put(row, text, state):
    """Add text to a row, a tab going to the next stop as it does in a terminal."""
    for c in text:
        if c == "\t":
            row += [(" ", dict(state))] * (8 - len(row) % 8)
        else:
            row.append((c, dict(state)))

def cells(text, fg0, bg0):
    """Each line as a list of (character, fg, bg, bold, dim, italic, underline, strike)."""
    out = []
    state = dict(fg=None, bg=None, bold=False, dim=False, italic=False, under=False, strike=False, rev=False)
    for line in text.split("\n"):
        row, at = [], 0
        for m in re.finditer(r"\x1b\[([0-9;:]*)m", line):
            put(row, line[at:m.start()], state)
            at = m.end()
            codes = [int(c) if c else 0 for c in re.split("[;:]", m.group(1))] or [0]
            i = 0
            while i < len(codes):
                c = codes[i]
                if c == 0: state.update(fg=None, bg=None, bold=False, dim=False, italic=False, under=False, strike=False, rev=False)
                elif c == 1: state["bold"] = True
                elif c == 2: state["dim"] = True
                elif c == 3: state["italic"] = True
                elif c == 4: state["under"] = True
                elif c == 7: state["rev"] = True
                elif c == 9: state["strike"] = True
                elif c == 22: state.update(bold=False, dim=False)
                elif c == 23: state["italic"] = False
                elif c == 24: state["under"] = False
                elif c == 27: state["rev"] = False
                elif c == 29: state["strike"] = False
                elif 30 <= c <= 37: state["fg"] = indexed(c - 30)
                elif 90 <= c <= 97: state["fg"] = indexed(c - 90 + 8)
                elif 40 <= c <= 47: state["bg"] = indexed(c - 40)
                elif 100 <= c <= 107: state["bg"] = indexed(c - 100 + 8)
                elif c == 39: state["fg"] = None
                elif c == 49: state["bg"] = None
                elif c in (38, 48):
                    key = "fg" if c == 38 else "bg"
                    if codes[i + 1] == 5:
                        state[key] = indexed(codes[i + 2]); i += 2
                    else:
                        rest = [x for x in codes[i + 2:i + 6]]
                        state[key] = tuple(rest[-3:] if len(rest) == 4 and ":" in m.group(1) else rest[:3]); i += 4 if not (len(rest) == 4 and ":" in m.group(1)) else 5
                i += 1
        put(row, re.sub(r"\x1b\[[0-9;:?]*[A-Za-z]", "", line[at:]), state)
        out.append(row)
    while out and not out[-1]:
        out.pop()
    return out

@functools.cache
def fallback(ch):
    """A font that has a character the terminal's own does not."""
    found = subprocess.run(["fc-match", "--format=%{file}", f"monospace:charset={ord(ch):x}"], capture_output=True, text=True).stdout
    return found or None

# Which sides of a cell a line-drawing character reaches: (up, right, down, left).
LINES = {"─": (0, 1, 0, 1), "│": (1, 0, 1, 0), "┌": (0, 1, 1, 0), "┐": (0, 0, 1, 1), "└": (1, 1, 0, 0), "┘": (1, 0, 0, 1),
         "├": (1, 1, 1, 0), "┤": (1, 0, 1, 1), "┬": (0, 1, 1, 1), "┴": (1, 1, 0, 1), "┼": (1, 1, 1, 1),
         "╭": (0, 1, 1, 0), "╮": (0, 0, 1, 1), "╰": (1, 1, 0, 0), "╯": (1, 0, 0, 1), "⎿": (1, 1, 0, 0)}
SHADES = {"█": 1.0, "▓": 0.72, "▒": 0.46, "░": 0.22}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--bg", default="#282828")
    ap.add_argument("--fg", default="#ebdbb2")
    ap.add_argument("--size", type=int, default=30)
    ap.add_argument("--pad", type=int, default=28)
    ap.add_argument("--cols", type=int, default=0)
    ap.add_argument("--rows", type=int, default=0, help="always this many rows: for the frames of a film")
    ap.add_argument("--square", action="store_true", help="no rounded corners or palette: a frame, not a picture")
    args = ap.parse_args()
    fg0, bg0 = rgb(args.fg), rgb(args.bg)
    grid = cells(sys.stdin.read(), fg0, bg0)
    if args.rows:
        grid = (grid + [[]] * args.rows)[:args.rows]
    cols = args.cols or max(len(r) for r in grid)
    font = {style: ImageFont.truetype(font_file(style), args.size) for style in STYLES}
    ascent, descent = font["Regular"].getmetrics()
    cw, ch = round(font["Regular"].getlength("M")), ascent + descent
    pad = args.pad
    img = Image.new("RGB", (cols * cw + 2 * pad, len(grid) * ch + 2 * pad), bg0)
    draw = ImageDraw.Draw(img)
    known = set()
    mix = lambda a, b, t: tuple(round(a[i] * t + b[i] * (1 - t)) for i in range(3))
    for y, row in enumerate(grid):
        for x, (c, s) in enumerate(row[:cols]):
            fg, bg = s["fg"] or fg0, s["bg"] or bg0
            if s["rev"]:
                fg, bg = bg, fg
            if s["dim"]:
                fg = mix(fg, bg, 0.6)
            x0, y0 = pad + x * cw, pad + y * ch
            if bg != bg0:
                draw.rectangle([x0, y0, x0 + cw - 1, y0 + ch - 1], fill=bg)
            if c == " ":
                pass
            elif c in LINES:
                up, right, down, left = LINES[c]
                w = max(2, args.size // 12)
                cx, cy = x0 + cw // 2, y0 + ch // 2
                if up: draw.rectangle([cx - w // 2, y0, cx - w // 2 + w - 1, cy + w // 2], fill=fg)
                if down: draw.rectangle([cx - w // 2, cy - w // 2, cx - w // 2 + w - 1, y0 + ch - 1], fill=fg)
                if left: draw.rectangle([x0, cy - w // 2, cx + w // 2, cy - w // 2 + w - 1], fill=fg)
                if right: draw.rectangle([cx - w // 2, cy - w // 2, x0 + cw - 1, cy - w // 2 + w - 1], fill=fg)
            elif c in SHADES:
                draw.rectangle([x0, y0 + ch // 8, x0 + cw - 1, y0 + ch - 1 - ch // 8], fill=mix(fg, bg, SHADES[c]))
            elif c == "▞":
                draw.rectangle([x0 + cw // 2, y0 + ch // 8, x0 + cw - 1, y0 + ch // 2 - 1], fill=fg)
                draw.rectangle([x0, y0 + ch // 2, x0 + cw // 2 - 1, y0 + ch - 1 - ch // 8], fill=fg)
            else:
                style = ("Bold" if s["bold"] else "") + ("Italic" if s["italic"] else "") or "Regular"
                f = font[style]
                if c not in known and font["Regular"].getmask(c).getbbox() is None or f.getlength(c) == 0:
                    other = fallback(c)
                    if other:
                        f = ImageFont.truetype(other, args.size)
                known.add(c)
                # A character from another font may be wider than a cell: set it in the middle.
                shift = (cw - f.getlength(c)) / 2 if f is not font[style] else 0
                draw.text((x0 + shift, y0), c, font=f, fill=fg)
            if s["under"]:
                draw.line([x0, y0 + ascent + 3, x0 + cw - 1, y0 + ascent + 3], fill=fg, width=max(1, args.size // 16))
            if s["strike"]:
                draw.line([x0, y0 + ch // 2 + 1, x0 + cw - 1, y0 + ch // 2 + 1], fill=fg, width=max(1, args.size // 16))
    if args.square:
        img.save(args.out)
        return
    # Rounded corners, like a window's.
    r = pad
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, img.size[0] - 1, img.size[1] - 1], radius=r, fill=255)
    img.putalpha(mask)
    # A screen is a few flat colours and the edges of letters: a small palette keeps the file small.
    img = img.quantize(colors=224, method=Image.Quantize.FASTOCTREE, dither=Image.Dither.NONE)
    img.save(args.out, optimize=True)
    print(f"{args.out}: {img.size[0]}x{img.size[1]}, {cols}x{len(grid)} cells")

main()
