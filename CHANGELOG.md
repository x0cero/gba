# Changelog

## 1.2.1 (2026-09-21)

- Continue adjoining map terrain beyond the engine's live margin, preventing false border trees across the Viridian City and Route 1 entrance.

- Reconstruct dense forest trees from complete original sprites and keep canopy artwork off the ground beneath them.
- Match Viridian's green-roof house tile patterns so roofs and facades form continuous buildings instead of plant billboards and isolated columns.
- Keep Mart counters and shelves at consistent heights, and preserve the register artwork without stretching it into a pillar.

- Preserve tree-layer transparency so raised ground-color rectangles cannot hide characters.
- Join alternate trunk-edge tiles to their crowns, keeping neighboring trees complete and consistently sized.

- Keep grass rustling effects from changing character bounds and producing a vertical bounce.
- Include walkable treetop tiles in complete tree billboards instead of leaving their crowns flat on the ground.

- Force FireRed battles into the original 2D renderer using the game's battle flag, preventing stale room geometry from covering combat and tutorial dialogue.
- Add battle-state replay with exact framebuffer checks and recovery to the overworld.

- Keep indoor back walls level when plants or fixtures interrupt their collision tiles, including the Viridian Pokémon Center.
- Render cave rock formations as terrain with their original tile textures, avoiding repeated house-roof strips.
- Add Center, Mart and Route 1 gameplay checks, cave geometry assertions and optional scenery review images to the ROM audit.

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
