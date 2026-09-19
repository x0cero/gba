//! Complete live overworld sprites, including actors culled by the 2D camera.
//! FireRed keeps their animation tiles and positions in gSprites while active.
use super::*;

pub(super) type Pixel = (i32, i32, u32, u8);
#[derive(Default)]
pub(super) struct Scene {
    pub pixels: Vec<Pixel>,
    pub anchors: [Option<(i32, i32)>; 16],
}

const EVENTS: usize = 0x36e38;
const SPRITES: usize = 0x2063c;

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn position(s: &[u8]) -> (i32, i32) {
    (
        word(s, 0x20) as i16 as i32 + word(s, 0x24) as i16 as i32 + s[0x28] as i8 as i32,
        word(s, 0x22) as i16 as i32 + word(s, 0x26) as i16 as i32 + s[0x29] as i8 as i32,
    )
}

fn dimensions(s: &[u8]) -> Option<(i32, i32)> {
    // Affine and 8bpp objects stay on the ordinary capture path.
    let a0 = word(s, 0);
    if a0 & 0x2100 != 0 || s[0x3e] & 1 == 0 {
        return None;
    }
    let size = word(s, 2) >> 14;
    Some(match (a0 >> 14, size) {
        (0, n) => (8 << n, 8 << n),
        (1, 0) => (16, 8),
        (1, 1) => (32, 8),
        (1, 2) => (32, 16),
        (1, 3) => (64, 32),
        (2, 0) => (8, 16),
        (2, 1) => (8, 32),
        (2, 2) => (16, 32),
        (2, 3) => (32, 64),
        _ => return None,
    })
}

pub(super) fn read(bus: &Bus, cap: &Capture, camera: (i32, i32)) -> Scene {
    let events = &bus.ewram[EVENTS..EVENTS + 16 * 0x24];
    let sprite = |e: &[u8]| {
        let start = SPRITES + e[4] as usize * 0x44;
        (e[4] < 64).then(|| &bus.ewram[start..start + 0x44])
    };
    let fallback = || Scene {
        pixels: cap
            .sprite_pixels
            .iter()
            .map(|&(x, y, c, id)| (x as i32, y as i32, c, id))
            .collect(),
        ..Scene::default()
    };
    // Calibrate the engine's world-to-screen offset from its visible player.
    // No frame history: warps and save-state loads cannot leave stale actors.
    let Some(player) = events
        .as_chunks::<0x24>()
        .0
        .iter()
        .find(|e| e[0] & 1 != 0 && e[2] & 1 != 0)
    else {
        return fallback();
    };
    let Some(ps) = sprite(player) else {
        return fallback();
    };
    let Some((pw, ph)) = dimensions(ps) else {
        return fallback();
    };
    let Some(oam) = bus.oam.as_chunks::<8>().0.iter().find(|&o| {
        word(o, 4) == word(ps, 4)
            && word(o, 0) & 0xff00 == word(ps, 0) & 0xff00
            && word(o, 2) & 0xfe00 == word(ps, 2) & 0xfe00
            && (word(o, 2) & 511) < 240
            && (word(o, 0) & 255) < 160
    }) else {
        return fallback();
    };
    let (px, py) = position(ps);
    let (sx, sy) = ((word(oam, 2) & 511) as i32, (word(oam, 0) & 255) as i32);
    let offset = (
        sx - if sx + pw > 512 { 512 } else { 0 } - px,
        sy - if sy + ph > 256 { 256 } else { 0 } - py,
    );
    // OAM and camera registers can be one update apart. A resting object's
    // tile coordinate provides a stable world origin for all engine sprites.
    let offset = events
        .as_chunks::<0x24>()
        .0
        .iter()
        .find_map(|e| {
            if e[0] & 1 == 0 || e[0x10..0x14] != e[0x14..0x18] {
                return None;
            }
            let (mx, my) = (word(e, 0x10) as i16 as i32, word(e, 0x12) as i16 as i32);
            if mx < 7 || my < 7 {
                return None;
            }
            let s = sprite(e)?;
            let (w, h) = dimensions(s)?;
            if s[0x3e] & 2 == 0 {
                return None;
            }
            let (x, y) = position(s);
            Some((
                (mx - 7) * 16 + 8 - (x + w / 2) - camera.0 + 112,
                (my - 7) * 16 + 15 - (y + h - 1) - camera.1 + 80,
            ))
        })
        .unwrap_or(offset);
    let mut anchors = [None; 16];
    let mut replaced = [false; 128];
    let mut actors = Vec::new();
    for (id, e) in events.as_chunks::<0x24>().0.iter().enumerate() {
        // Honor story invisibility, but ignore the game's narrow-screen cull.
        if e[0] & 1 == 0 || e[1] & 0x20 != 0 {
            continue;
        }
        let Some(s) = sprite(e) else { continue };
        let Some((w, h)) = dimensions(s) else {
            continue;
        };
        if s[0x3e] & 4 != 0 && e[1] & 0x40 == 0 {
            continue;
        }
        let (mut x, mut y) = position(s);
        if s[0x3e] & 2 != 0 {
            x += offset.0;
            y += offset.1;
        }
        if x < -96 - w || x > 336 || y < -96 - h || y > 256 {
            continue;
        }
        let a2 = word(s, 4);
        for (i, o) in bus.oam.as_chunks::<8>().0.iter().enumerate() {
            if word(o, 4) == a2
                && word(o, 0) & 0xff00 == word(s, 0) & 0xff00
                && word(o, 2) & 0xfe00 == word(s, 2) & 0xfe00
            {
                replaced[i] = true;
            }
        }
        anchors[id] = Some((x + w / 2, y + h - 1));
        actors.push((y, x, w, h, id, s));
    }
    actors.sort_by_key(|a| a.0);
    let mut pixels = Vec::new();
    for (y, x, w, h, id, s) in actors {
        let a1 = word(s, 2);
        let a2 = word(s, 4);
        let stride = if bus.io[0] & 0x40 != 0 {
            w as usize / 8
        } else {
            32
        };
        for dy in 0..h {
            for dx in 0..w {
                let tx = if a1 & 0x1000 != 0 { w - 1 - dx } else { dx } as usize;
                let ty = if a1 & 0x2000 != 0 { h - 1 - dy } else { dy } as usize;
                let tile = ((a2 as usize & 1023) + ty / 8 * stride + tx / 8) & 1023;
                let byte = bus.vram[0x10000 + tile * 32 + ty % 8 * 4 + tx % 8 / 2];
                let index = (byte >> ((tx & 1) * 4)) & 15;
                if index == 0 {
                    continue;
                }
                let c = word(
                    &bus.palette,
                    0x200 + (a2 as usize >> 12) * 32 + index as usize * 2,
                ) as u32;
                let channel = |shift: u32| {
                    let v = (c >> shift) & 31;
                    (v << 3) | (v >> 2)
                };
                let color = channel(0) << 16 | channel(5) << 8 | channel(10);
                pixels.push((x + dx, y + dy, color, 128 + id as u8));
            }
        }
    }
    // Preserve field effects (including grass covering a character's legs).
    pixels.extend(
        cap.sprite_pixels
            .iter()
            .filter(|p| !replaced[p.3 as usize])
            .map(|&(x, y, c, id)| (x as i32, y as i32, c, id)),
    );
    Scene { pixels, anchors }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(bytes: &mut [u8], at: usize, value: i32) {
        bytes[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
    }

    fn scene() -> Bus {
        let mut bus = Bus::new(vec![0; 192]);
        bus.ewram[EVENTS] = 1;
        bus.ewram[EVENTS + 2] = 1; // player
        bus.ewram[SPRITES + 0x3e] = 3;
        put(&mut bus.ewram, SPRITES, 0x8000);
        put(&mut bus.ewram, SPRITES + 2, 0x8000);
        put(&mut bus.ewram, SPRITES + 0x20, 112);
        put(&mut bus.ewram, SPRITES + 0x22, 56);
        put(&mut bus.oam, 0, 0x8038);
        put(&mut bus.oam, 2, 0x8070);
        bus.io[0] = 0x40;
        bus.vram[0x10000..0x10400].fill(0x11);
        put(&mut bus.palette, 0x202, 31);
        bus.ewram[EVENTS + 0x24] = 1;
        bus.ewram[EVENTS + 0x24 + 4] = 1;
        let player = bus.ewram[SPRITES..SPRITES + 0x44].to_vec();
        bus.ewram[SPRITES + 0x44..SPRITES + 0x88].copy_from_slice(&player);
        put(&mut bus.ewram, SPRITES + 0x44 + 4, 8);
        bus
    }

    #[test]
    fn whole_actor_survives_each_original_screen_edge_and_engine_culling() {
        let mut bus = scene();
        for (x, y) in [(-8, 60), (235, 60), (100, -16), (100, 155), (100, 180)] {
            put(&mut bus.ewram, SPRITES + 0x44 + 0x20, x);
            put(&mut bus.ewram, SPRITES + 0x44 + 0x22, y);
            bus.ewram[EVENTS + 0x24 + 1] = 0x40; // offScreen
            bus.ewram[SPRITES + 0x44 + 0x3e] = 7; // hidden by engine
            let pixels: Vec<_> = read(&bus, &Capture::default(), (0, 0))
                .pixels
                .into_iter()
                .filter(|p| p.3 == 129)
                .collect();
            assert_eq!(pixels.len(), 16 * 32);
            assert_eq!(pixels.iter().map(|p| p.0).min(), Some(x));
            assert_eq!(pixels.iter().map(|p| p.1).max(), Some(y + 31));
            assert!(pixels.iter().all(|p| p.2 == 0xff0000));
        }
    }

    #[test]
    fn story_hidden_and_removed_actors_stay_hidden_and_effects_survive() {
        let mut bus = scene();
        let mut cap = Capture::default();
        cap.sprite_pixels.push((120, 80, 0xabcdef, 2));
        for flags in [0, 0x2001] {
            put(&mut bus.ewram, EVENTS + 0x24, flags);
            let pixels = read(&bus, &cap, (0, 0)).pixels;
            assert!(!pixels.iter().any(|p| p.3 == 129));
            assert!(pixels.contains(&(120, 80, 0xabcdef, 2)));
        }
    }

    #[test]
    fn unsupported_scene_uses_capture_without_retaining_previous_actors() {
        let mut bus = scene();
        assert!(!read(&bus, &Capture::default(), (0, 0)).pixels.is_empty());
        bus.ewram[EVENTS] = 0;
        assert!(read(&bus, &Capture::default(), (0, 0)).pixels.is_empty());
    }

    #[test]
    fn resting_map_coordinate_prevents_one_frame_oam_camera_drift() {
        let mut bus = scene();
        for at in [0x10, 0x12, 0x14, 0x16] {
            put(&mut bus.ewram, EVENTS + at, 14);
        }
        let first = read(&bus, &Capture::default(), (120, 120));
        // Hardware OAM can still contain the previous frame's scroll.
        put(&mut bus.oam, 2, 0x8071);
        let second = read(&bus, &Capture::default(), (120, 120));
        assert_eq!(first.anchors, second.anchors);
        assert_eq!(first.pixels, second.pixels);
        let moved = read(&bus, &Capture::default(), (121, 120));
        assert_eq!(moved.anchors[1].unwrap().0, first.anchors[1].unwrap().0 - 1);
    }
}
