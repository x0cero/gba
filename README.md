# gba

[![CI](https://github.com/x0cero/gba/actions/workflows/ci.yml/badge.svg)](https://github.com/x0cero/gba/actions/workflows/ci.yml)

Game Boy Advance emulator written from scratch in Rust. Runs Pokémon FireRed, frame-identical to mGBA.

**[Play it in your browser](https://x0cero.github.io/gba/)**: the same Rust core compiled to WebAssembly. Drop in your own `.gba` file, or click "Run CPU test suite" to see the emulator work without one.

No emulation libraries and no ported reference code: the ARM7TDMI core, the PPU, the DMA controller, the timers, the save hardware and the audio path were each built against the hardware documentation and test ROMs, one failing case at a time. No BIOS image is required; the BIOS calls games actually make are implemented in high-level Rust.

![Pokémon FireRed running in this emulator, walking around Pallet Town](screenshots/firered-demo.gif)

![Pokémon FireRed title screen](screenshots/firered-title.png)

![Pokémon FireRed, Pallet Town overworld from a real save](screenshots/firered-pallet-town.png)

![Kirby title screen](screenshots/title.png) ![Kirby gameplay](screenshots/gameplay.png) ![jsmolka test suite passing](screenshots/tests.png)

Screenshots are framebuffer dumps from this emulator (Pokémon FireRed is © Game Freak and Nintendo, Kirby: Nightmare in Dream Land is © HAL Laboratory and Nintendo; no ROMs are included or distributed).

## Accuracy

The standout claim is not "it boots" but "it matches". Pokémon FireRed was run for 9,000 frames (about two and a half minutes of gameplay) under a scripted input sequence, and every frame was compared against mGBA running the identical script. The output was pixel-for-pixel identical the whole way through.

The methodology is reproducible from this repo. `tools/refprobe.c` links against libmgba (installed via `brew install mgba`) and drives mGBA with the same scripted input this emulator accepts through `GBA_INPUT`. Both sides dump their framebuffer, palette RAM, OAM and VRAM at the same frame numbers, and the dumps are diffed. When they diverge, the first differing frame plus the register-level dumps point at the exact hardware behavior that is wrong. Most of the harder bugs in this project (the DMA address latching, the affine sprite transform, the IntrWait flag semantics) were found this way rather than by staring at code.

On the standard suites, the CPU core passes the jsmolka `arm`, `thumb` and `memory` tests in full.

[docs/differential-testing.md](docs/differential-testing.md) is a full writeup of the harness, the bisect method, the bugs it caught, and what differential testing cannot catch.

## Features

- **ARM7TDMI CPU**: the complete ARM and Thumb instruction sets, mode banking, the SPSR/CPSR machinery, and interrupt dispatch.
- **PPU**: modes 0 through 5, text and affine backgrounds, regular and affine sprites, the two windows plus the object window, alpha blending and brightness effects, and scanline-accurate timing.
- **BIOS high-level emulation**: `CpuSet`/`CpuFastSet`, `Div`/`Sqrt`/`ArcTan2`, LZ77, RLE, Huffman and BitUnPack decompression, `ObjAffineSet`/`BgAffineSet`, and `IntrWait`/`VBlankIntrWait` with the real flag semantics. You do not need to supply a BIOS dump.
- **DMA** in all four channels covering immediate, vblank, hblank and sound-FIFO timing, with hardware-correct address latching (the registers are write-only and the internal pointers advance independently).
- **Timers** with cascade, and the keypad including its interrupt.
- **Audio**: both DirectSound FIFO channels plus all four PSG channels, mixed with output headroom and a low-pass filter, paced off the audio clock so the picture does not drift against the sound.
- **Every common save medium, auto-detected**: 32 KB SRAM, 64 KB flash (Panasonic), 128 KB two-bank flash (Sanyo), and bit-serial EEPROM over DMA3 in both the 512 byte and 8 KB variants. The medium is inferred from the SDK marker string left in the ROM, and the emulator prints which one it chose at startup.
- **Quality of life**: save states, pause, and hold-to-fast-forward.
- **iOS frontend**: a SwiftUI app in `ios/` that drives the same Rust core through a C FFI layer, with touch controls, a game library, and MFi controller support.

## Building and running

```sh
cargo build --release
./target/release/gba path/to/rom.gba
```

No ROMs are included in this repository, and none ever will be. The test ROMs used during development are [jsmolka/gba-tests](https://github.com/jsmolka/gba-tests), which CI clones on every run.

### Download

Prebuilt binaries for macOS (Apple Silicon), Linux (x86_64), and Windows (x86_64) are attached to each tagged release on the [releases page](https://github.com/x0cero/gba/releases). Download the archive for your platform, unpack it, and run the binary with a ROM path.

Battery saves are written to `<rom>.gba.sav` next to the ROM, sized to whatever save chip the cartridge actually declares. Save states go to `<rom>.gba.state`.

## Controls

| GBA | Keyboard |
|-----|----------|
| A / B | Z / X |
| Start / Select | Enter / Right Shift |
| D-pad | Arrow keys |
| L / R | Q / W |
| Save state / load state | F5 / F7 |
| Pause | P |
| Fast-forward (hold) | Tab |
| Quit | Esc |

## Architecture

- `src/cpu.rs`: the ARM7TDMI core. Fetch, decode, execute, the pipeline, mode switching, and interrupt entry.
- `src/bus.rs`: the memory map and everything hanging off it. DMA, timers, the interrupt controller, the keypad, save-chip emulation, the BIOS high-level calls, and the DirectSound mixer.
- `src/ppu.rs`: the scanline renderer and the LCD state machine.
- `src/psg.rs`: the four legacy Game Boy sound channels.
- `src/lib.rs`: the C FFI surface that the iOS app links against.
- `src/wasm.rs`: the wasm-bindgen surface behind the browser demo, built by `scripts/build-wasm.sh` into `web/pkg/`.
- `web/`: the browser frontend (canvas, keyboard, Web Audio worklet), published to GitHub Pages.
- `ios/`: the SwiftUI frontend over that FFI.

## Verification and debugging tools

Everything below is driven by environment variables, so a failing case can be reproduced headlessly and diffed.

- `GBA_FRAMES=N ./target/release/gba rom.gba --headless` renders N frames and writes `frame.ppm`.
- `GBA_INPUT="100-110:a,200-260:up"` scripts button presses by frame range, which is what makes deterministic playthroughs and mGBA comparisons possible.
- `GBA_DUMP_EVERY=K` with `GBA_DUMP_DIR` writes a filmstrip of frames so you can find the exact frame where a picture goes wrong.
- `GBA_WAV=1` captures the last ten seconds of audio to `samples.raw` as 32-bit float stereo.
- Targeted trace hooks: `GBA_SWILOG`, `GBA_IOLOG`, `GBA_MODELOG`, `GBA_BOOTTRACE`, `GBA_PALTRACE`, `GBA_BADPTR`, `GBA_OBJDEBUG`, `GBA_BREAK`, `GBA_SAVELOG`.

## Status and known gaps

These are honest trade-offs, documented rather than hidden.

- **Timing is region-aware, not cycle-accurate.** Instructions are costed by which memory region the program counter is in (1 cycle in IWRAM, 3 in EWRAM, 4 in ROM). There is no prefetch buffer and no sub-instruction memory timing. Games that lean on exact cycle counts rather than the standard interrupts will misbehave.
- **No serial or link cable, and no real-time clock.** FireRed needs neither, but games that do (the Ruby/Sapphire berry clock, any link trading) will not work.
- **Some SWI calls are unimplemented.** The set games actually reach is covered; the rest fall through rather than being emulated.
- **Modes 1 and 2 affine backgrounds are lightly exercised**, since the games tested here do not use them heavily.
- **A ROM with no save marker at all falls back to 128 KB flash**, which is a guess rather than a detection.
- **Test coverage is narrow.** `cargo test` covers the save controllers (medium detection, the flash command protocol, the EEPROM bit protocol, and `.sav` sizing). The wider accuracy claim rests on the test ROM suites and the mGBA comparison, not on unit tests.

## License

MIT. See [LICENSE](LICENSE).
