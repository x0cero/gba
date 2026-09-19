# 3D mode: how it works, what is tested, how to tune it

`--3d` is an alpha. This page is the detail behind the short section in the README.

## What it does

The game runs exactly as it does in 2D. The 3D renderer changes how each frame is drawn:

- It reads the live map grid out of the game's memory every frame (the map layout, the metatile attributes and the border block), so the geometry always matches the town the player is standing in. Nothing is modelled by hand.
- Blocked cells (buildings, fences, furniture) are extruded from the map. Trees stand as whole artwork units. Windows and doors stay on building fronts.
- Characters stay sprites. Active overworld characters are read from the game's object-event list, including the ones outside the original 240x160 screen. A tinted marker shows the player when scenery hides them.
- Indoors, bookcases combine their cap, shelves and base into solid cabinets, and supported back walls stand upright. Oak's lab has hand-verified profiles for its table, round machine and book stands. Every other piece of furniture goes through the general tile classifier, which is a guess.
- Battles and full-screen menus fall back to plain 2D. Dialogue is overlaid on the scene.

The renderer compares the map it predicts against the pixels the game actually drew. When they disagree (battle wipes, warp fades, the title screen), it shows the flat 2D frame for that moment.

## Two styles

Default: the original FireRed artwork on map-based geometry.

```sh
./target/release/gba path/to/firered.gba --3d
```

Opt-in experiment: polygon trees and pitched roofs, procedurally modelled.

```sh
GBA_3D_STYLE=modeled ./target/release/gba path/to/firered.gba --3d
```

## What has been checked

Only the early game has been looked at closely.

| Area | Status |
|------|--------|
| Pallet Town | Checked, in the regression suite |
| Oak's lab | Checked, in the regression suite |
| Viridian City, Pokémon Center, Mart, Route 1 | Checked, in the regression suite |
| Everything else | Rendered by the audit, not played through |

The audit below runs every map layout in the ROM through the scenery renderer, but it uses a reconstructed map grid and a synthetic character. It proves the masking and depth do not crash or leak, not that every town looks right. Expect wrong shapes in later towns, caves and unusual buildings.

## Tuning variables

| Variable | Meaning | Default |
|----------|---------|---------|
| `GBA_3D_STYLE` | `sprite` (original art) or `modeled` | `sprite` |
| `GBA_3D_PERSP` | Lens. `3` is flat, `1` is the old wide lens | `3` |
| `GBA_TILT` | Tilt-shift blur, `0` to `3` | `2` for sprite, `0` for modeled |
| `GBA_3D_WALL` | Wall height. `1` is half height | `1` |
| `GBA_MARGIN` | Fading at the screen edge | |
| `GBA_TREE_TALL` | Tree height multiplier | |
| `GBA_WOOD` | Depth shading on tree trunks | |

## Regression suite

`tools/regress3d.py` boots a ROM headlessly, walks a scripted route and checks camera motion, figure placement, warps, post-processing and determinism. Run it after any renderer change:

```sh
GBA_3D_WALL=1 python3 tools/regress3d.py --rom /tmp/firered.gba
```

It needs a battery save that starts in Pallet Town. Use a copy of the ROM and save, because exiting the emulator overwrites the `.sav` next to the ROM.

For the Viridian checks, pass `--viridian-rom /path/to/rom.gba` as well, or `--only viridian`. That fixture needs a save outside Viridian's Pokémon Center from before Oak's parcel. The checks walk through the Center, the Mart and the connection to Route 1. Every run works on temporary copies.

With `GBA_3D_STYLE=modeled` the suite also checks model stability.

## ROM layout audit

The audit reads a local US FireRed ROM without opening or writing saves, discovers every layout record (used and unused), loads its tile graphics and palettes, and checks silhouette masking at the centre and four corners under both indoor and outdoor boundary rules:

```sh
GBA_AUDIT_ROM=/path/to/firered.gba cargo test --release --bin gba all_rom_layouts -- --ignored --nocapture
```

It also exercises cave terrain separately, checking that rock formations never use building roof or facade geometry.

Set `GBA_AUDIT_IMAGES=/tmp/map-review` and `GBA_AUDIT_LAYOUTS=2e55cc,2fa890` to export flat and 3D scenery views for those hexadecimal ROM layout offsets. Offsets are ROM-specific. Exports contain no NPCs or scripted changes.

What the audit does not cover: live NPC behaviour, connected-map transitions, animated tiles, or whether each furniture shape is visually correct.

## Debug traces

- `GBA_3D_TRACE=1` prints the camera position and map size per frame.
- `GBA_3D_GEOM=1` prints the geometry the renderer built.
- `tools/treediff.py` pixel-diffs trees between the 2D and 3D output.
- `GBA_3D_GUESS=1` restores the old colour-classifier fallback when no map grid is readable.

## Known gaps

- Only FireRed is supported. The map and art decoding assume the FRLG ROM layout.
- Furniture outside Oak's lab and the Viridian interiors is classified by heuristic.
- Characters the game has completely unloaded can pop in when they load.
- The 3D renderer is native desktop only. The browser demo stays 2D.
- Floating-point rasteriser, not bit-exact against any hardware.

Found a town that renders wrong? Open an issue with the [3D rendering bug](https://github.com/x0cero/gba/issues/new?template=3d-rendering-bug.md) template: town name, screenshot, ROM version.
