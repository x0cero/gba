//! Optional ROM-backed scenery audit. Never executes or writes a game save.
use super::*;

fn word(rom: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(rom[at..at + 4].try_into().unwrap())
}
fn offset(rom: &[u8], ptr: u32) -> Option<usize> {
    ptr.checked_sub(0x0800_0000)
        .map(|x| x as usize)
        .filter(|&x| x < rom.len())
}
fn unpack(rom: &[u8], at: usize) -> Vec<u8> {
    assert_eq!(rom[at], 0x10, "expected LZ77 tiles");
    let len = word(rom, at) as usize >> 8;
    assert!(len <= 0x8000);
    let mut out = Vec::with_capacity(len);
    let mut pos = at + 4;
    while out.len() < len {
        let flags = rom[pos];
        pos += 1;
        for bit in (0..8).rev() {
            if out.len() == len {
                break;
            }
            if flags & (1 << bit) == 0 {
                out.push(rom[pos]);
                pos += 1;
            } else {
                let a = rom[pos] as usize;
                let b = rom[pos + 1] as usize;
                pos += 2;
                let distance = ((a & 15) << 8 | b) + 1;
                for _ in 0..((a >> 4) + 3).min(len - out.len()) {
                    out.push(out[out.len() - distance]);
                }
            }
        }
    }
    out
}

#[test]
#[ignore = "requires GBA_AUDIT_ROM pointing to a local FireRed ROM"]
fn all_rom_layouts_scenery_and_silhouette() {
    let rom = std::fs::read(std::env::var("GBA_AUDIT_ROM").expect("set GBA_AUDIT_ROM")).unwrap();
    assert_eq!(&rom[0xac..0xb0], b"BPRE", "FireRed US only");
    // Discover layout records by their full structure, not a revision-specific
    // table address. Include unused layouts as well as live maps.
    let mut layouts = Vec::new();
    for at in (0..rom.len() - 28).step_by(4) {
        let (w, h) = (word(&rom, at), word(&rom, at + 4));
        if !(1..=512).contains(&w) || !(1..=512).contains(&h) {
            continue;
        }
        let ptrs: Option<Vec<usize>> = [8, 12, 16, 20]
            .iter()
            .map(|i| offset(&rom, word(&rom, at + i)))
            .collect();
        let Some(p) = ptrs else {
            continue;
        };
        if p[1] + (w * h * 2) as usize > rom.len()
            || !(1..=16).contains(&rom[at + 24])
            || !(1..=16).contains(&rom[at + 25])
        {
            continue;
        }
        if ![p[2], p[3]].iter().enumerate().all(|(secondary, &t)| {
            t + 24 <= rom.len()
                && rom[t] <= 1
                && rom[t + 1] == secondary as u8
                && [4, 8, 12, 20]
                    .iter()
                    .all(|i| offset(&rom, word(&rom, t + i)).is_some())
        }) {
            continue;
        }
        layouts.push(at);
    }
    assert!(
        layouts.len() >= 300,
        "incomplete layout discovery: {}",
        layouts.len()
    );
    let mut bus = Bus::new(rom);
    let mut renderer = Renderer::new();
    let mut reference = Renderer::new();
    let mut scenes = 0;
    let mut hidden_pixels = 0;
    let mut visible_pixels = 0;
    for &layout in &layouts {
        let (w, h) = (
            word(&bus.rom, layout) as usize,
            word(&bus.rom, layout + 4) as usize,
        );
        let data = offset(&bus.rom, word(&bus.rom, layout + 12)).unwrap();
        bus.vram.fill(0);
        bus.palette.fill(0);
        for secondary in 0..2 {
            let t = offset(&bus.rom, word(&bus.rom, layout + 16 + secondary * 4)).unwrap();
            let tiles = offset(&bus.rom, word(&bus.rom, t + 4)).unwrap();
            let pal = offset(&bus.rom, word(&bus.rom, t + 8)).unwrap();
            let gfx = if bus.rom[t] != 0 {
                unpack(&bus.rom, tiles)
            } else {
                bus.rom[tiles..tiles + if secondary == 0 { 0x5000 } else { 0x3000 }].to_vec()
            };
            let start = secondary * 0x5000;
            assert!(start + gfx.len() <= 0x8000);
            bus.vram[start..start + gfx.len()].copy_from_slice(&gfx);
            let range = if secondary == 0 {
                0..7 * 32
            } else {
                7 * 32..13 * 32
            };
            for i in range {
                bus.palette[i] = bus.rom[pal + i];
            }
        }
        // Reconstruct the game's padded map grid without booting or warping.
        let stride = w + 15;
        assert!(stride * (h + 15) * 2 < 0x30000);
        bus.ewram[..stride * (h + 15) * 2].fill(0xff);
        for y in 0..h {
            for x in 0..w {
                let dst = ((y + 7) * stride + x + 7) * 2;
                let src = data + (y * w + x) * 2;
                bus.ewram[dst..dst + 2].copy_from_slice(&bus.rom[src..src + 2]);
            }
        }
        bus.write32(0x0300_5040, stride as u32);
        bus.write32(0x0300_5044, (h + 15) as u32);
        bus.write32(0x0300_5048, 0x0200_0000);
        bus.write32(0x0300_5008, 0x0203_0000);
        bus.write32(0x0203_6DFC, 0x0800_0000 + layout as u32);
        bus.write32(0x0203_6E08, 0);
        // Exercise both boundary policies for every layout. This also avoids
        // relying on inferred map-header identity for shared/unused layouts.
        for kind in [1, 8] {
            bus.write8(0x0203_6E13, kind);
            for (px, py) in [
                (w / 2, h / 2),
                (0, 0),
                (w - 1, 0),
                (0, h - 1),
                (w - 1, h - 1),
            ] {
                bus.write16(0x0203_0000, px as u16);
                bus.write16(0x0203_0002, py as u16);
                reset_history();
                let grid = MapGrid::read(&bus, None).expect("layout did not produce a grid");
                renderer.zbuf.fill(f32::INFINITY);
                renderer.buffer.fill(0x333333);
                renderer.render_world(&grid);
                assert!(renderer.zbuf.iter().all(|z| !z.is_nan() && *z > 0.0));
                renderer.scenery_depth.copy_from_slice(&renderer.zbuf);
                let (mx, my) = (px as i32 * 16 + 8, py as i32 * 16 + 16);
                let (wx, wz) = grid.world_of_map(mx, my);
                let ground = grid.ground_at_map(mx, my - 8);
                let point = |x, height| {
                    let (a, b, c) = project(x, ground + height * COS_P, wz + height * SIN_P);
                    (a, b, c - 2.0)
                };
                let q = [
                    point(wx - 8.0, 24.0),
                    point(wx + 8.0, 24.0),
                    point(wx + 8.0, 0.0),
                    point(wx - 8.0, 0.0),
                ];
                let sample = |u: f32, v: f32| {
                    if !(0.2..=0.8).contains(&u) && v > 0.7 {
                        SKIP
                    } else {
                        0xff0000
                    }
                };
                reference.zbuf.fill(f32::INFINITY);
                reference.quad_uv(q, &mut sample.clone());
                renderer.counting = true;
                renderer.cov = 0;
                renderer.vis = 0;
                renderer.quad_uv(q, &mut sample.clone());
                renderer.counting = false;
                let before = renderer.buffer.clone();
                renderer.quad_uv_ghost(q, &mut sample.clone(), Ghost::Marker);
                for (i, &old) in before.iter().enumerate() {
                    if !reference.zbuf[i].is_finite() {
                        continue;
                    }
                    if reference.zbuf[i] <= renderer.scenery_depth[i] + 0.01 {
                        assert_eq!(
                            renderer.buffer[i], old,
                            "false marker: layout {layout:x}, {px},{py}, type {kind}"
                        );
                        visible_pixels += 1;
                    } else {
                        assert_ne!(
                            renderer.buffer[i], old,
                            "missing marker: layout {layout:x}, {px},{py}, type {kind}"
                        );
                        hidden_pixels += 1;
                    }
                }
                scenes += 1;
            }
        }
    }
    assert!(visible_pixels > 0 && hidden_pixels > 0);
    println!(
        "AUDIT {} layouts, {scenes} scenes, {visible_pixels} visible and {hidden_pixels} hidden sprite pixels",
        layouts.len()
    );
}
