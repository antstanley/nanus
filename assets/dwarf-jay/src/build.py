#!/usr/bin/env python3
"""Regenerate the dwarf jay SVG and PNG assets from the sketches in this folder.

Needs Google Chrome (or any Chromium) and Python 3, and nothing else: no packages, no network
beyond the p5 CDN script the preview sketches load. Point CHROME at the binary if it is not the
macOS default.

    python3 build.py svg        # ../svg/*.svg, from export-svg.html and lockup-svg.html
    python3 build.py png        # ../png/*.png, transparent, 2x, rendered from the SVGs
    python3 build.py previews   # ../previews/*.png, the contact sheets
    python3 build.py all
"""
import html, os, re, subprocess, sys, tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parent
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
FLAGS = ["--headless=new", "--hide-scrollbars", "--force-device-scale-factor=1"]


def chrome(*args, **kw):
    return subprocess.run([CHROME, *FLAGS, *args], capture_output=True, text=True, timeout=240, **kw)


def build_svg():
    """Each generator page writes its SVGs into <pre id="svg-NAME"> elements; read them from the DOM."""
    (OUT / "svg").mkdir(exist_ok=True)
    count = 0
    for page in ("export-svg.html", "lockup-svg.html"):
        dom = chrome("--window-size=800,600", "--virtual-time-budget=30000", "--dump-dom", (HERE / page).as_uri()).stdout
        for name, body in re.findall(r'<pre id="svg-([a-z_-]+)">(.*?)</pre>', dom, re.S):
            (OUT / "svg" / f"dwarf-jay-{name}.svg").write_text(html.unescape(body) + "\n")
            count += 1
    print(f"{count} svg files")


def build_png():
    (OUT / "png").mkdir(exist_ok=True)
    for svg in sorted((OUT / "svg").glob("*.svg")):
        w, h = (int(float(v)) for v in re.search(r'width="([\d.]+)" height="([\d.]+)"', svg.read_text()).groups())
        with tempfile.TemporaryDirectory() as tmp:
            page = Path(tmp) / "p.html"
            page.write_text(f'<body style="margin:0;background:transparent"><img src="{svg.as_uri()}" width="{w}" height="{h}" style="display:block">')
            target = OUT / "png" / (svg.stem + ".png")
            subprocess.run([CHROME, "--headless=new", "--hide-scrollbars", "--default-background-color=00000000",
                            "--force-device-scale-factor=2", f"--window-size={w},{h}", "--virtual-time-budget=10000",
                            f"--screenshot={target}", page.as_uri()], capture_output=True, timeout=120)
        print("png", target.name)


def shot(url, target, size):
    chrome(f"--window-size={size}", "--virtual-time-budget=40000", f"--screenshot={target}", url)
    print("preview", Path(target).name)


def build_previews():
    (OUT / "previews").mkdir(exist_ok=True)
    tools = (HERE / "tool-logos.html").as_uri()
    shot(tools, OUT / "previews" / "tool-logos-light.png", "1600,1650")
    shot(tools + "?dark=1", OUT / "previews" / "tool-logos-dark.png", "1600,1650")
    with tempfile.TemporaryDirectory() as tmp:
        page = Path(tmp) / "lockup.html"
        cells = "".join(
            f'<div style="background:{bg}"><img src="{(OUT / "svg" / f).as_uri()}" width="800"></div>'
            for bg in ("#ecebf0", "#1b1f3a") for f in ("dwarf-jay-lockup.svg", "dwarf-jay-lockup-sticker.svg"))
        page.write_text(f'<body style="margin:0;display:grid;grid-template-columns:repeat(2,800px)">{cells}</body>')
        shot(page.as_uri(), OUT / "previews" / "lockups.png", "1600,1600")


if __name__ == "__main__":
    steps = sys.argv[1:] or ["all"]
    for s in steps:
        if s in ("svg", "all"): build_svg()
        if s in ("png", "all"): build_png()
        if s in ("previews", "all"): build_previews()
