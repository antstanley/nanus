# Dwarf jay: design guide

The dwarf jay is nanus's mascot: one small, slate-blue jay, drawn as a die-cut comic sticker. This
folder holds the finished artwork, the sketches that generate it, and the rules the artwork follows,
so a new logo can be added without re-deriving them.

## What is here

```
assets/dwarf-jay/
  DESIGN.md            this guide
  svg/                 15 vector logos (transparent background)
  png/                 the same 15, rendered transparent at 2x
  previews/            contact sheets on light and dark backgrounds
  src/                 the generators (p5.js sketches and a build script)
```

| File | What it is |
|---|---|
| `svg/dwarf-jay-lockup-sticker.svg` | **The primary logo.** The jay sticker with a `DWARFJAY` banner as a second sticker laid over it. 800 x 800. |
| `svg/dwarf-jay-lockup.svg` | The same lockup as a single sticker: one outline round the jay and the banner. 800 x 800. |
| `svg/dwarf-jay-plain.svg` | The default jay: no tool, no wordmark. Use it wherever the mascot stands alone. |
| `svg/dwarf-jay-<tool>.svg` | One jay per nanus tool, holding that tool's analogue. No wordmark. 428 x 299. |

`png/` mirrors `svg/` name for name. A PNG of a sticker looks as if it has no border on a white page,
because the border is white: put it on a tinted or dark ground to see the shape.

## The character

A jay, in profile, facing right, perched with its tail down and to the left. It was traced from a
reference photograph of a slate-blue jay and simplified to a handful of smooth shapes.

- **Proportions.** Plump body, long tail, and a head enlarged to 120% of the photograph's, so the face
  reads at sticker size.
- **Face.** A black mask with a small white-ringed eye and a pale throat patch. Short, dark, pointed
  beak. A pale-blue crown stripe.
- **Shading.** Flat colour with two cel tones, a halftone-dot belly, and thin lighter highlight strokes
  on the back, wing and tail edge. No gradients.
- **Line.** A near-black outline, round joins and caps, about 3.4 units at the 400-unit sticker scale.
- **Mood.** Eyes change; the body does not. The default is a plain open eye.

### Palette

| Role | Hex |
|---|---|
| Outline, mask | `#14141c`, `#15151d` |
| Body, head | `#5b87d6` |
| Underside shade | `#4268b3` |
| Wing | `#2f58a8` |
| Tail | `#23418a` |
| Crown stripe | `#a8cdf6` |
| Throat | `#e4eef9` |
| Beak | `#2c2c32` |
| Back, wing and tail highlights | `#9dbdf0`, `#8fb6f2`, `#4d97ff` |
| Legs and toes | `#5b4f57` |
| Claws | `#e3d9c4` |
| Sticker border | `#ffffff` |

Backgrounds the art was checked against: `#ecebf0` (light) and `#1b1f3a` (dark).

### Legs and claws

Both legs share one style: a scaled, dark-brown leg with a few scale ticks, an ankle joint, three
tapered forward toes and one short hind toe. Each toe ends in a **short, rounded, cream claw**. Sharp
dark hooks were tried and rejected as too aggressive.

A jay that holds a tool lifts one leg: it leaves the lower chest, bends at a visible knee, and the toes
wrap the tool's shaft. The other leg hangs. Tools are held in the **claws**, never the wings.

## The sticker

Every logo is a die-cut sticker. The rules are the same everywhere:

1. **One outline.** The silhouette is every shape in the artwork stroked white, about 26 units wide
   at sticker scale (about 13 units of border) with round joins.
2. **No gaps.** Slits and small holes in the silhouette are closed, so the border is one continuous
   piece. In the SVGs this is a blur-and-threshold filter on the border group; props with a hollow, such
   as the magnet, get a solid white backing so the hollow fills.
3. **Detached details join.** A sparkle, a motion tick or a hovering needle gets a wider border radius
   so it merges with the main outline instead of floating on its own. Nothing is a separate island.
4. **Shadow.** A copy of the silhouette, black at 32% opacity, offset 6 right and 8 down at sticker scale
   (9 and 11 in the 800-unit lockups). It sits under the border and is never blurred.
5. **Stay inside the canvas.** Props are placed so the border is never clipped.

## The tool logos

nanus offers the model seven tools and five goal tools. Each gets a jay holding the real-world object
that does the same job. The metaphors are lateral on purpose: the aim is a tool you can recognise at a
glance, not a literal picture of a file.

| Tool | Analogue | Prop and accents |
|---|---|---|
| `read` | the librarian | open book; gold-framed spectacles |
| `write` | the quill | a blue jay feather; an ink squiggle; happy eyes |
| `edit` | the red pencil | proofreader's pencil with a pink eraser; a pink sparkle |
| `read_image` | the photographer | camera with a flash sparkle; happy eyes |
| `glob` | the sieve | wire sieve with an orange `*` wildcard on the mesh |
| `grep` | needle, meet magnet | red horseshoe magnet with a needle hovering over its poles |
| `bash` | the hammer | hammer, yellow hard hat, impact ticks, grumpy brow |
| `get_goal` | check the compass | brass compass |
| `create_goal` | plant the flag | green pennant with a gold star; a sparkle |
| `update_goal` | stamped: done | rubber stamp; a green check seal |
| `pause_goal` | coffee break | steaming mug; sleepy eyes |
| `abandon_goal` | white flag | white flag held clear of the body; a sweat drop; worried brow |
| (none) | the default | the bare jay |

The goal tools are the loop's own and are not in the registry; they are drawn in the same style so the
set reads as one family. Props are drawn on a shared grid: the grip is at the origin and the handle runs
up from it, so a new prop only needs its own shape and one entry in the hold table.

Prop colours are chosen to stand apart from the jay's blues: orange, red, gold, green and cream.

### Adding a tool logo

1. Add the prop to `PROPS` in `src/tool-logos.html`, drawn with its grip at the origin.
2. Add a `HOLD` entry if it needs a different angle or grip point than the default stick.
3. Add a row to `STK` with the tool name, a short analogy and a caption colour.
4. Add the name to `NAMES` in `src/export-svg.html` (the order must match `STK`).
5. Run `python3 src/build.py all` and look at the previews.

## The wordmark

`DWARFJAY`, one word, all capitals, set as hand-built block letters (not a font) so the artwork is the
same wherever it is opened.

- **Letters.** Heavy, angular and bevelled, sheared forward by 0.22, stretched 1.18 wide by 1.45 tall.
  Each letter bounces on its own baseline by up to 9 units and tilts by up to 1.7 degrees, for the
  jumpy comic feel. Letter shapes are polygons with even-odd counters (`D`, `A`, `R`).
- **Fill.** Yellow `#ffd21a`, orange `#ff8a1f` across the lower 45%, with a thin white highlight bar near
  the top.
- **Outline and extrusion.** A 6-unit near-black outline over a seven-step extruded shadow in dark red
  `#8a1426`, offset down and right.
- **Banner.** A jagged red `#e5373b` caption strip with faint diagonal light streaks and a darker
  bottom edge, tilted about -2.9 degrees, with three black speed slashes trailing off the first letter
  and three yellow impact ticks off the last.
- **Lockup.** The jay is drawn at 1.9x. The banner is scaled to 74%, moved to the right of centre and
  raised so it crosses the jay's flank; the claws and the end of the tail show below it.

### Sticker on sticker

In `dwarf-jay-lockup-sticker.svg` the banner is its own sticker: its own border and its own drop
shadow, laid over the jay's. The shadow of the banner falls on the jay. This is the preferred
lockup. The single-outline version exists for places where two stacked stickers would be too busy.

## SVG notes

- Backgrounds are transparent; the white border and the shadow are part of the artwork.
- The die-cut border and the shadow use two SVG filters (`close` and `shade`). Browsers, Inkscape and
  most renderers support them. A tool that ignores filters will lose the gap-closing and the shadow,
  and the artwork will still be correct, only without those two effects.
- All single logos share one `viewBox` so they line up when placed side by side.
- Shapes are paths, so the files scale freely. There is no live text and no embedded font.

## Regenerating

Everything in `svg/`, `png/` and `previews/` is generated. Requirements: Python 3 and Google Chrome (or
set `CHROME` to any Chromium). The sketches load p5.js from a CDN.

```sh
python3 assets/dwarf-jay/src/build.py all       # or: svg, png, previews
```

| Source | Role |
|---|---|
| `src/tool-logos.html` | The p5.js sheet of all 13 jays on one page. `?dark=1` for the dark ground. |
| `src/claw-options.html` | The p5.js comparison of hand and claw treatments (kept as a record of the choice; `?zoom=1` for close-ups). |
| `src/export-svg.html` | The same drawing code recorded into SVG paths. Produces the 13 single-logo SVGs. |
| `src/lockup-svg.html` | The lockups: the jay, the wordmark and both banner arrangements. |
| `src/build.py` | Runs Chrome headlessly to extract the SVGs, render the PNGs and the previews. |

The three SVG-producing sketches share one drawing routine. The canvas versions draw to a 2D context,
and the SVG versions swap in a recording context that writes the same calls out as paths, so the two
cannot drift apart.

## Not decided

- A version with no white border, for places that cannot show a sticker.
- Whether the goal tools get a shared accent (for example a gold frame) to mark them as a group.
- Animated versions. Nothing here moves.
