# Dwarf jay — fixed-view takeoff

Six seconds, 144 frames at 24 fps. The bird pauses, crouches, spreads its wings,
pushes off, tucks its feet, and flaps out of the upper-left edge. There is no
camera movement, zoom, or perspective change. The branch, woodland, and captions
are identical in every frame. The loop pauses on the empty scene before restarting.

## View

- `dwarf-jay-takeoff.gif`: broadly compatible looping preview, 720 × 720.
- `dwarf-jay-takeoff.mp4`: full-resolution 960 × 960 video, six seconds.
- `dwarf-jay-takeoff.svg`: standalone vector animation, 960 × 960 viewBox,
  73 discrete SMIL animation channels. Open in a modern browser. Static vector
  editors may show only the first frame; use `svg-frames/` for individual poses.
- `index.html`: browser player with pause, replay, and frame scrubbing. If opening
  local files prevents the browser accessing the SVG timeline, serve this folder:

  ```sh
  python3 -m http.server 8000 --directory artifacts/dwarf-jay/takeoff
  ```

  Then visit http://localhost:8000/ . Reduced-motion preferences pause autoplay
  in this player; use Play to start it.
- `contact-sheet.png`: twelve sample poses, chronological left-to-right, top-to-bottom.
- `frames/`: all 144 full-resolution rendered PNG frames.
- `svg-frames/`: all 144 static, editable SVG frames.
- `pose-000.vectorcraft`, `pose-044.vectorcraft`: editable perched and airborne poses.

## Reproduce

From the repository root, with `vectorcraft-cli` and FFmpeg installed:

```sh
python3 artifacts/dwarf-jay/animate.py
ffmpeg -framerate 24 -i artifacts/dwarf-jay/takeoff/frames/frame-%03d.png \
  -c:v libx264 -crf 18 -pix_fmt yuv420p -movflags +faststart \
  artifacts/dwarf-jay/takeoff/dwarf-jay-takeoff.mp4
ffmpeg -framerate 24 -i artifacts/dwarf-jay/takeoff/frames/frame-%03d.png \
  -vf 'scale=720:720:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=192:stats_mode=full[p];[s1][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle' \
  -loop 0 artifacts/dwarf-jay/takeoff/dwarf-jay-takeoff.gif
```

The generator opens the original native illustration through
`vectorcraft-cli mcp --headless`, reads original geometry, adds a far wing, and
poses every frame using MCP `path.setAnchors` commands. Every PNG and static SVG
frame is exported through the MCP server. The animated SVG keys those same path
coordinates at the same frame times. FFmpeg packages the PNG sequence, without
camera transforms or interpolation.

## Checks performed

- 144 PNG and 144 SVG frames; six-second, 24 fps MP4.
- Fixed-scene SVG subtrees are identical to the original in all 144 frames.
- 73 vector animation channels with matching 145-entry value/time lists.
- No raster images or external font dependencies in the animated SVG.
- MCP exports completed without warnings.
- Headless Chrome timeline check: the eye moves between perched and airborne
  poses, and is entirely beyond the left page edge at five seconds.
- Inspected contact-sheet poses and an independently rendered in-flight SVG frame.
