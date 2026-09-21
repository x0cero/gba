# Development

## Building

```sh
cargo build --release
./target/release/gba path/to/rom.gba
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs the build, the tests, clippy and the [jsmolka/gba-tests](https://github.com/jsmolka/gba-tests) suites, which it clones on every run. No ROMs are in this repository and none ever will be.

The browser build is `scripts/build-wasm.sh`, which writes `web/pkg/` (gitignored). GitHub Pages deploys `web/` on every push to `master`.

Releases are cut by tagging: `git tag vX.Y.Z && git push origin vX.Y.Z`. The release workflow builds macOS arm64, Linux x86_64 and Windows x86_64 binaries and attaches them to the release. Create the release with its notes first (`gh release create vX.Y.Z --target master --notes-file ...`) so the workflow has somewhere to attach files.

## Layout

| Path | What it is |
|------|------------|
| `src/cpu.rs` | ARM7TDMI core: fetch, decode, execute, pipeline, mode switching, interrupt entry |
| `src/bus.rs` | Memory map and everything on it: DMA, timers, interrupt controller, keypad, save chips, BIOS high-level calls, DirectSound mixer |
| `src/ppu.rs` | Scanline renderer and LCD state machine, plus per-frame layer capture for 3D |
| `src/psg.rs` | The four legacy Game Boy sound channels |
| `src/voxel.rs` | 3D mode: map decoding, camera tracking, rasterisation |
| `src/voxel/actors.rs` | Overworld sprites and positions from FireRed's live object events |
| `src/voxel/connections.rs` | Adjacent-map terrain beyond the live grid margin |
| `src/voxel/interiors.rs` | Indoor cabinets, back walls, the lab table profile |
| `src/voxel/models.rs` | Procedural trees and buildings for `GBA_3D_STYLE=modeled` |
| `src/voxel/map_audit.rs` | The ROM layout audit |
| `src/lib.rs` | C FFI surface the iOS app links against |
| `src/wasm.rs` | wasm-bindgen surface behind the browser demo |
| `web/` | Browser frontend: canvas, keyboard, touch controls, Web Audio worklet, PWA |
| `ios/` | SwiftUI frontend over the FFI (`xcodegen` builds the project from `project.yml`) |
| `tools/` | `refprobe.c` (mGBA differential probe), `regress3d.py`, `treediff.py` |
| `docs/` | This folder |

## Headless runs and traces

Everything is driven by environment variables so a failing case can be reproduced without a window and diffed.

- `GBA_FRAMES=N ./target/release/gba rom.gba --headless` renders N frames and writes `frame.ppm`.
- `GBA_INPUT="100-110:a,200-260:up"` scripts button presses by frame range. This is what makes deterministic playthroughs and mGBA comparisons possible.
- `GBA_DUMP_EVERY=K` with `GBA_DUMP_DIR` writes a filmstrip of frames.
- `GBA_WAV=1` captures the last ten seconds of audio to `samples.raw` (32-bit float stereo).
- Trace hooks: `GBA_SWILOG`, `GBA_IOLOG`, `GBA_MODELOG`, `GBA_BOOTTRACE`, `GBA_PALTRACE`, `GBA_BADPTR`, `GBA_OBJDEBUG`, `GBA_BREAK`, `GBA_SAVELOG`.

Two things that bite: the ROM path must come before flags (`gba rom.gba --3d`, not the other way round), and exiting overwrites `<rom>.sav` next to the ROM, so run headless jobs against a copy in `/tmp`.

## Accuracy testing

[differential-testing.md](differential-testing.md) describes the mGBA comparison harness: the same scripted input through both emulators, framebuffer plus VRAM, palette and OAM dumped at the same frame numbers, and diffed. FireRed matched pixel for pixel over 9,000 frames. `tools/refprobe.c` is the mGBA side; it links against libmgba (`brew install mgba`).

[3d-mode.md](3d-mode.md) covers the 3D regression suite and the ROM layout audit.
