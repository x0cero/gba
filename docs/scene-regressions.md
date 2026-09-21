# Scene regression fixtures

Use local ROMs and states only. Do not commit or distribute them.

## Rival battle

Save a FireRed US state during the first rival battle, then run:

```sh
GBA_BATTLE_STATE=/path/to/battle.state cargo test --release --bin gba rival_battle_frames -- --ignored --nocapture
```

The test advances text, selects the first attack and plays until the overworld returns. Every battle frame must reproduce the original framebuffer at the desktop renderer's 3x scale. It also requires at least 60 recovered overworld frames. This fixture tests combat and its exit, not starter selection or the earlier battle-entry transition.

For screenshots, copy the ROM into a temporary directory, run there, and set `GBA_LOAD_STATE=/path/to/battle.state` with `--headless --3d`. The state embeds its ROM; use a matching ROM path. Headless execution writes dumps and a battery save beside the temporary ROM. `GBA_INPUT` and `GBA_DUMP_EVERY` can drive and capture the replay.

FireRed retains map data in RAM during battle and uses display mode 0 for both scenes. Map pointers, display mode and pixel matching cannot alone establish that the field is active. The renderer checks `gMain.inBattle` first. The flag layout is documented in [pret's FireRed reconstruction](https://github.com/pret/pokefirered/blob/master/include/main.h).

## Route 1 grass and trees

Run `python3 tools/regress3d.py --only viridian --viridian-rom /path/to/rom.gba` with a save outside Viridian's Center. The harness uses temporary copies so the source save is preserved. It walks south into Route 1 and then north through tall grass. The stride check requires 15 stride frames with grass beneath the actor during the crossing, a grounded actor, unchanged sprite width and at most two pixels of footline variation. The tree check requires complete tip, canopy and trunk tiles at four known Route 1 trees across moving camera frames.

Grass effects previously expanded the merged figure's bounds, moving its apparent feet by ten pixels. Actor bounds now come from the reconstructed character sprite. Walkable treetop tiles are grouped with the blocked canopy below using their artwork and atlas relationship. These checks cover the reproduced route issues, not every field effect or wild-battle entry animation.

The tree-edge replay walks east and west behind a row of three trees. At rest beside the third crown, frames 2624 through 2640 must keep at least 95% of the player visible. Earlier moving frames can correctly pass behind the crown and are reviewed visually rather than required to remain unobscured. The reproduced old build kept only 79.2% visible at rest, while the corrected build keeps 98.6%. Tree units must include alternate trunk-edge tiles as well as their tips. Transparent tree-layer pixels must not be replaced with opaque ground pixels on the raised billboard.


The Viridian fixture also walks to the two green-roof houses and verifies all five columns share the same two roof rows, two facade rows and height. The house rule requires the complete roof/wall tile pattern, not merely green pixels. Mart furniture checks cover each full counter and shelving unit; its profile requires the room dimensions and five matching tiles. These are verified building/furniture families, not universal recognition of every building and interior.

Dense primary-tileset forest fragments use the original complete tree artwork at their existing anchors. The forest floor uses the map's prevalent plain green walkable tile, excluding partial overlays and tall-grass behavior. Review the connected city/route entrance visually as well as the Pallet border; geometry and palette checks alone do not catch sliced canopies.

The connected-terrain replay crosses from Viridian into Route 1 and back. Route 1's grass at (11, 7..8), equivalent to Viridian (23, 47..48), must remain grass on both sides of the handoff. Previously these cells became repeating border trees when seen from the city, beyond the engine's short live neighbor strip. The renderer now follows the map connection's offset and destination layout for terrain beyond that strip. Live cells retain priority. Only matching tilesets are decoded from the neighboring layout; unavailable art remains empty. This covers immediate neighboring maps, not recursive traversal through multiple map connections.
