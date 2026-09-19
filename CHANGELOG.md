# Changelog

## 1.2.0 (2026-09-19)

### 3D rendering

- Render complete active FireRed characters beyond the original viewport, reducing clipped figures and animation-driven position drift.
- Keep the original tree artwork and building textures as the default. Procedural trees and pitched roofs are an optional experiment through `GBA_3D_STYLE=modeled`.
- Correct building roof and facade sampling, perspective texture interpolation, and map border dimensions.
- Assemble indoor bookcases into cabinets and improve Oak's lab walls, equipment, tables and desk items.
- Restrict hidden-player markers to pixels covered by scenery. Visible characters no longer become translucent simply by standing near furniture.
- Reset camera history and queued audio when loading a save state.
- Reuse headless renders for capture so screenshot dumping does not advance renderer history twice.

### Verification

- 35 standard unit tests and 19 scripted gameplay checks.
- 270 silhouette cases covering depth, transparency, screen clipping and foreground characters.
- Optional local-ROM audit passed 336 layouts and 3,360 scene configurations. This checks masking with reconstructed scenery and a synthetic character, not full gameplay on every map.

The 3D mode is desktop-only and experimental. Unloaded distant NPCs can still pop in, and some scenery shapes need further work. No ROMs or saves are included.

## 1.1.0 (2026-09-18)

- First public desktop 3D diorama mode for FireRed, with map-based scenery, sprite characters and original artwork.

## 1.0.0 (2026-08-09)

- First public release with desktop binaries and the Rust GBA emulator core.
