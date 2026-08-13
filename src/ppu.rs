/// GBA PPU: 240x160, scanline renderer. Supports text backgrounds (modes
/// 0-1), affine backgrounds (modes 1-2), bitmap modes 3-5, and sprites
/// (regular + affine). Blending and window effects are not yet implemented.
pub const WIDTH: usize = 240;
pub const HEIGHT: usize = 160;

fn rgb555(c: u16) -> u32 {
    let r = (c & 0x1F) as u32;
    let g = (c >> 5 & 0x1F) as u32;
    let b = (c >> 10 & 0x1F) as u32;
    (r << 19 | r >> 2 << 16) | (g << 11 | g >> 2 << 8) | (b << 3 | b >> 2)
}

#[derive(Clone, Copy, Default)]
struct ObjPixel {
    color: u16,
    prio: u8,
    opaque: bool,
    semi: bool,   // OAM mode 1: semi-transparent
    window: bool, // OAM mode 2: contributes to the object window
    /// OAM index of the object that wrote this pixel. Only the --3d capture
    /// uses it, to tell two touching figures apart.
    obj: u8,
}

fn blend(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let comp = |sh: u16| {
        let ca = (a >> sh & 0x1F) as u32;
        let cb = (b >> sh & 0x1F) as u32;
        ((ca * eva + cb * evb) / 16).min(31) as u16
    };
    comp(0) | comp(5) << 5 | comp(10) << 10
}

fn brighten(a: u16, evy: u32) -> u16 {
    let comp = |sh: u16| {
        let c = (a >> sh & 0x1F) as u32;
        (c + (31 - c) * evy / 16) as u16
    };
    comp(0) | comp(5) << 5 | comp(10) << 10
}

fn darken(a: u16, evy: u32) -> u16 {
    let comp = |sh: u16| {
        let c = (a >> sh & 0x1F) as u32;
        (c - c * evy / 16) as u16
    };
    comp(0) | comp(5) << 5 | comp(10) << 10
}

#[derive(bincode::Encode, bincode::Decode)]
pub struct Ppu {
    pub framebuffer: [u32; WIDTH * HEIGHT],
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            framebuffer: [0; WIDTH * HEIGHT],
        }
    }

    fn pal16(palette: &[u8], bank: u32, idx: u32, obj: bool) -> u16 {
        let base = if obj { 0x200 } else { 0 };
        let off = base + bank as usize * 32 + idx as usize * 2;
        u16::from_le_bytes([palette[off], palette[off + 1]])
    }

    fn pal256(palette: &[u8], idx: u32, obj: bool) -> u16 {
        let base = if obj { 0x200 } else { 0 };
        u16::from_le_bytes([
            palette[base + idx as usize * 2],
            palette[base + idx as usize * 2 + 1],
        ])
    }

    /// Render one scanline from the current register/VRAM state.
    /// `io` is the raw I/O register block; `bg_ref` the per-frame affine
    /// reference point accumulators (updated by the caller each line).
    pub fn render_scanline(
        &mut self,
        y: u32,
        io: &[u8],
        palette: &[u8],
        vram: &[u8],
        oam: &[u8],
        bg_ref: &[(i32, i32); 2],
    ) {
        let r16 = |off: usize| u16::from_le_bytes([io[off], io[off + 1]]);
        let dispcnt = r16(0);
        let backdrop = rgb555(u16::from_le_bytes([palette[0], palette[1]]));
        let row = &mut self.framebuffer[y as usize * WIDTH..(y as usize + 1) * WIDTH];

        if dispcnt & 0x80 != 0 {
            row.fill(rgb555(0x7FFF)); // forced blank: white
            return;
        }
        row.fill(backdrop);

        // Per-layer line buffers: BG layers as raw RGB555 (0x8000 flag =
        // opaque), sprites with priority and attributes.
        let mut bg_line = [[0u16; WIDTH]; 4]; // bit15 set = opaque pixel
        let mut obj_line = [ObjPixel::default(); WIDTH];
        if dispcnt & 0x1000 != 0 {
            Self::render_sprites(y, dispcnt, palette, vram, oam, &mut obj_line);
        }
        Self::draw_bg_layers(y, dispcnt, io, palette, vram, bg_ref, &mut bg_line);

        // Window configuration. Layer visibility masks per region.
        let win0_on = dispcnt & 0x2000 != 0;
        let win1_on = dispcnt & 0x4000 != 0;
        let objwin_on = dispcnt & 0x8000 != 0;
        let any_window = win0_on || win1_on || objwin_on;
        let winin = r16(0x48);
        let winout = r16(0x4A);
        let win_range = |h: u16, v: u16| -> (u32, u32, u32, u32) {
            let x1 = (h >> 8) as u32;
            let x2 = (h & 0xFF) as u32;
            let y1 = (v >> 8) as u32;
            let y2 = (v & 0xFF) as u32;
            (x1, x2.min(WIDTH as u32), y1, y2)
        };
        let (w0x1, w0x2, w0y1, w0y2) = win_range(r16(0x40), r16(0x44));
        let (w1x1, w1x2, w1y1, w1y2) = win_range(r16(0x42), r16(0x46));
        let in_vrange = |y1: u32, y2: u32| {
            if y1 <= y2 {
                y >= y1 && y < y2
            } else {
                y >= y1 || y < y2
            }
        };
        let w0v = win0_on && in_vrange(w0y1, w0y2);
        let w1v = win1_on && in_vrange(w1y1, w1y2);

        // Blending configuration.
        let bldcnt = r16(0x50);
        let blend_mode = bldcnt >> 6 & 3;
        let bldalpha = r16(0x52);
        let eva = (bldalpha & 0x1F).min(16) as u32;
        let evb = (bldalpha >> 8 & 0x1F).min(16) as u32;
        let evy = (r16(0x54) & 0x1F).min(16) as u32;
        let backdrop = u16::from_le_bytes([palette[0], palette[1]]);

        if y == 106 && std::env::var("GBA_OBJDEBUG").is_ok() {
            let n = obj_line.iter().filter(|p| p.opaque).count();
            eprintln!("line106: obj opaque={} dispcnt={:04X}", n, dispcnt);
        }
        for x in 0..WIDTH {
            let xu = x as u32;
            // Determine layer-enable mask and effect-enable for this pixel.
            let (mask, effects) = if any_window {
                let in_h = |x1: u32, x2: u32| {
                    if x1 <= x2 {
                        xu >= x1 && xu < x2
                    } else {
                        xu >= x1 || xu < x2
                    }
                };
                if w0v && in_h(w0x1, w0x2) {
                    (winin & 0x3F, winin & 0x20 != 0)
                } else if w1v && in_h(w1x1, w1x2) {
                    (winin >> 8 & 0x3F, winin & 0x2000 != 0)
                } else if objwin_on && obj_line[x].window {
                    (winout >> 8 & 0x3F, winout & 0x2000 != 0)
                } else {
                    (winout & 0x3F, winout & 0x20 != 0)
                }
            } else {
                (0x3F, true)
            };

            // Find top and second pixel: (color, layer id 0-3 BG, 4 OBJ, 5 backdrop).
            let mut top = (backdrop, 5u16);
            let mut second = (backdrop, 5u16);
            let mut top_set = false;
            let obj = &obj_line[x];
            'search: for prio in 0..4u8 {
                // OBJ above BGs of the same priority.
                if obj.opaque && obj.prio == prio && mask & 0x10 != 0 {
                    if !top_set {
                        top = (obj.color, 4);
                        top_set = true;
                    } else {
                        second = (obj.color, 4);
                        break 'search;
                    }
                }
                for bg in 0..4usize {
                    if mask & (1 << bg) == 0 || dispcnt & (1 << (8 + bg)) == 0 {
                        continue;
                    }
                    let bgcnt = r16(0x8 + bg * 2);
                    if (bgcnt & 3) as u8 != prio {
                        continue;
                    }
                    let p = bg_line[bg][x];
                    if p & 0x8000 != 0 {
                        if !top_set {
                            top = (p & 0x7FFF, bg as u16);
                            top_set = true;
                        } else {
                            second = (p & 0x7FFF, bg as u16);
                            break 'search;
                        }
                    }
                }
            }

            if y == 106 && x == 100 && std::env::var("GBA_OBJDEBUG").is_ok() {
                eprintln!(
                    "px100: obj(op={} prio={} semi={} col={:04X}) top=({:04X},{}) second=({:04X},{}) mask={:02X} effects={} bldcnt={:04X}",
                    obj.opaque,
                    obj.prio,
                    obj.semi,
                    obj.color,
                    top.0,
                    top.1,
                    second.0,
                    second.1,
                    mask,
                    effects,
                    bldcnt
                );
            }
            // Apply color effects.
            let t1 = bldcnt & (1 << top.1) != 0;
            let t2 = bldcnt & (1 << (8 + second.1)) != 0;
            let semi = top.1 == 4 && obj.semi;
            let final555 = if effects && (semi || blend_mode == 1) && (semi || t1) && t2 {
                blend(top.0, second.0, eva, evb)
            } else if effects && blend_mode == 2 && t1 {
                brighten(top.0, evy)
            } else if effects && blend_mode == 3 && t1 {
                darken(top.0, evy)
            } else {
                top.0
            };
            row[x] = rgb555(final555);
        }
    }

    /// Fill the four per-layer line buffers for line `y` (bit 15 = opaque).
    /// Shared by normal compositing and the --3d layer capture.
    #[allow(clippy::too_many_arguments)]
    fn draw_bg_layers(
        y: u32,
        dispcnt: u16,
        io: &[u8],
        palette: &[u8],
        vram: &[u8],
        bg_ref: &[(i32, i32); 2],
        bg_line: &mut [[u16; WIDTH]; 4],
    ) {
        let mode = dispcnt & 7;
        for bg in 0..4u32 {
            if dispcnt & (1 << (8 + bg)) == 0 {
                continue;
            }
            let buf = &mut bg_line[bg as usize];
            match (mode, bg) {
                (0, _) | (1, 0) | (1, 1) => Self::draw_text_bg(y, bg, io, palette, vram, buf),
                (1, 2) | (2, 2) | (2, 3) => {
                    Self::draw_affine_bg(y, bg, io, palette, vram, buf, bg_ref)
                }
                (3, 2) => {
                    for x in 0..WIDTH {
                        let off = (y as usize * WIDTH + x) * 2;
                        buf[x] = u16::from_le_bytes([vram[off], vram[off + 1]]) | 0x8000;
                    }
                }
                (4, 2) => {
                    let base = if dispcnt & 0x10 != 0 { 0xA000 } else { 0 };
                    for x in 0..WIDTH {
                        let pi = vram[base + y as usize * WIDTH + x] as u32;
                        if pi != 0 {
                            buf[x] = Self::pal256(palette, pi, false) | 0x8000;
                        }
                    }
                }
                (5, 2) => {
                    if (y as usize) < 128 {
                        let base = if dispcnt & 0x10 != 0 { 0xA000 } else { 0 };
                        for x in 0..160.min(WIDTH) {
                            let off = base + (y as usize * 160 + x) * 2;
                            buf[x] = u16::from_le_bytes([vram[off], vram[off + 1]]) | 0x8000;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn draw_text_bg(y: u32, bg: u32, io: &[u8], palette: &[u8], vram: &[u8], row: &mut [u16]) {
        let r16 = |off: usize| u16::from_le_bytes([io[off], io[off + 1]]) as u32;
        let bgcnt = r16(0x8 + bg as usize * 2);
        let char_base = (bgcnt >> 2 & 3) as usize * 0x4000;
        let screen_base = (bgcnt >> 8 & 0x1F) as usize * 0x800;
        let eight_bpp = bgcnt & 0x80 != 0;
        let size = bgcnt >> 14 & 3;
        let hofs = r16(0x10 + bg as usize * 4) & 0x1FF;
        let vofs = r16(0x12 + bg as usize * 4) & 0x1FF;
        let (w_tiles, h_tiles) = match size {
            0 => (32, 32),
            1 => (64, 32),
            2 => (32, 64),
            _ => (64, 64),
        };
        let py = (y + vofs) % (h_tiles * 8);
        for x in 0..WIDTH as u32 {
            let px = (x + hofs) % (w_tiles * 8);
            // Screen blocks are 32x32 tiles; larger maps chain blocks.
            let sbb = match size {
                0 => 0,
                1 => px / 256,
                2 => py / 256,
                _ => (px / 256) + (py / 256) * 2,
            } as usize;
            let tx = (px / 8) % 32;
            let ty = (py / 8) % 32;
            let entry_off = screen_base + sbb * 0x800 + (ty * 32 + tx) as usize * 2;
            let entry = u16::from_le_bytes([vram[entry_off], vram[entry_off + 1]]) as u32;
            let tile = entry & 0x3FF;
            let mut fx = px % 8;
            let mut fy = py % 8;
            if entry & 0x400 != 0 {
                fx = 7 - fx;
            }
            if entry & 0x800 != 0 {
                fy = 7 - fy;
            }
            let color = if eight_bpp {
                let off = char_base + tile as usize * 64 + (fy * 8 + fx) as usize;
                if off >= 0x10000 { 0 } else { vram[off] as u32 }
            } else {
                let off = char_base + tile as usize * 32 + (fy * 4 + fx / 2) as usize;
                if off >= 0x10000 {
                    0
                } else {
                    let b = vram[off] as u32;
                    if fx & 1 == 0 { b & 0xF } else { b >> 4 }
                }
            };
            if color != 0 {
                let c = if eight_bpp {
                    Self::pal256(palette, color, false)
                } else {
                    Self::pal16(palette, entry >> 12, color, false)
                };
                row[x as usize] = c | 0x8000;
            }
        }
    }

    fn draw_affine_bg(
        _y: u32,
        bg: u32,
        io: &[u8],
        palette: &[u8],
        vram: &[u8],
        row: &mut [u16],
        bg_ref: &[(i32, i32); 2],
    ) {
        let r16 = |off: usize| u16::from_le_bytes([io[off], io[off + 1]]) as u32;
        let bgcnt = r16(0x8 + bg as usize * 2);
        let char_base = (bgcnt >> 2 & 3) as usize * 0x4000;
        let screen_base = (bgcnt >> 8 & 0x1F) as usize * 0x800;
        let wrap = bgcnt & 0x2000 != 0;
        let size_tiles: u32 = 16 << (bgcnt >> 14 & 3); // 128..1024 px / 8
        let size_px = (size_tiles * 8) as i32;
        let base = 0x20 + (bg as usize - 2) * 0x10;
        let pa = r16(base) as u16 as i16 as i32;
        let pc = r16(base + 4) as u16 as i16 as i32;
        let (mut rx, mut ry) = bg_ref[bg as usize - 2];
        for x in 0..WIDTH {
            let sx = rx >> 8;
            let sy = ry >> 8;
            rx += pa;
            ry += pc;
            let (sx, sy) = if wrap {
                (sx.rem_euclid(size_px), sy.rem_euclid(size_px))
            } else {
                if sx < 0 || sy < 0 || sx >= size_px || sy >= size_px {
                    continue;
                }
                (sx, sy)
            };
            let tile =
                vram[screen_base + (sy as u32 / 8 * size_tiles + sx as u32 / 8) as usize] as usize;
            let off = char_base + tile * 64 + (sy as usize % 8) * 8 + sx as usize % 8;
            if off < 0x10000 {
                let ci = vram[off] as u32;
                if ci != 0 {
                    row[x] = Self::pal256(palette, ci, false) | 0x8000;
                }
            }
        }
    }

    fn put_obj(px: &mut ObjPixel, color: u16, prio: u8, mode: u32, obj: u8) {
        if mode == 2 {
            px.window = true;
            return;
        }
        px.color = color;
        px.prio = prio;
        px.opaque = true;
        px.semi = mode == 1;
        px.obj = obj;
    }

    fn render_sprites(
        y: u32,
        dispcnt: u16,
        palette: &[u8],
        vram: &[u8],
        oam: &[u8],
        out: &mut [ObjPixel; WIDTH],
    ) {
        let one_dim = dispcnt & 0x40 != 0;
        let bitmap_mode = dispcnt & 7 >= 3;
        for i in (0..128).rev() {
            let a0 = u16::from_le_bytes([oam[i * 8], oam[i * 8 + 1]]) as u32;
            let a1 = u16::from_le_bytes([oam[i * 8 + 2], oam[i * 8 + 3]]) as u32;
            let a2 = u16::from_le_bytes([oam[i * 8 + 4], oam[i * 8 + 5]]) as u32;
            let affine = a0 & 0x100 != 0;
            if !affine && a0 & 0x200 != 0 {
                continue; // disabled
            }
            let shape = a0 >> 14 & 3;
            let size = a1 >> 14 & 3;
            let (w, h): (u32, u32) = match (shape, size) {
                (0, s) => (8 << s, 8 << s),
                (1, 0) => (16, 8),
                (1, 1) => (32, 8),
                (1, 2) => (32, 16),
                (1, 3) => (64, 32),
                (2, 0) => (8, 16),
                (2, 1) => (8, 32),
                (2, 2) => (16, 32),
                _ => (32, 64),
            };
            let obj_mode = a0 >> 10 & 3; // 0 normal, 1 semi-transparent, 2 obj-window
            if obj_mode == 3 {
                continue;
            }
            let double = affine && a0 & 0x200 != 0;
            let (bw, bh) = if double { (w * 2, h * 2) } else { (w, h) };
            let sy = a0 & 0xFF;
            let sx = a1 & 0x1FF;
            let oy = if sy + bh > 256 {
                sy as i32 - 256
            } else {
                sy as i32
            };
            let ox = if sx + bw > 512 {
                sx as i32 - 512
            } else {
                sx as i32
            };
            let line = y as i32 - oy;
            if line < 0 || line >= bh as i32 {
                continue;
            }
            let eight_bpp = a0 & 0x2000 != 0;
            let tile = a2 & 0x3FF;
            let prio = (a2 >> 10 & 3) as u8;
            let pal_bank = a2 >> 12;
            // In bitmap modes tiles 0-511 overlap the framebuffer; skip them.
            if bitmap_mode && tile < 512 {
                continue;
            }
            let row_tiles = if one_dim {
                w / 8 * if eight_bpp { 2 } else { 1 }
            } else {
                32
            };

            let sample = |tx: u32, ty: u32| -> u32 {
                // Tile data for sprites lives at 0x10000; entries are 32
                // bytes (4bpp). 8bpp uses even tile numbers.
                let t = tile + (ty / 8) * row_tiles + (tx / 8) * if eight_bpp { 2 } else { 1 };
                let base = 0x10000 + (t as usize & 0x3FF) * 32;
                if eight_bpp {
                    let off = base + (ty as usize % 8) * 8 + tx as usize % 8;
                    if off >= 0x18000 { 0 } else { vram[off] as u32 }
                } else {
                    let off = base + (ty as usize % 8) * 4 + (tx as usize % 8) / 2;
                    if off >= 0x18000 {
                        0
                    } else {
                        let b = vram[off] as u32;
                        if tx & 1 == 0 { b & 0xF } else { b >> 4 }
                    }
                }
            };

            if affine {
                let idx = (a1 >> 9 & 0x1F) as usize;
                let p = |n: usize| {
                    i16::from_le_bytes([oam[idx * 32 + 6 + n * 8], oam[idx * 32 + 7 + n * 8]])
                        as i32
                };
                let (pa, pb, pc, pd) = (p(0), p(1), p(2), p(3));
                let cx = bw as i32 / 2;
                let cy = bh as i32 / 2;
                for dx in 0..bw as i32 {
                    let x = ox + dx;
                    if !(0..WIDTH as i32).contains(&x) {
                        continue;
                    }
                    let tx = ((pa * (dx - cx) + pb * (line - cy)) >> 8) + w as i32 / 2;
                    let ty = ((pc * (dx - cx) + pd * (line - cy)) >> 8) + h as i32 / 2;
                    if tx < 0 || ty < 0 || tx >= w as i32 || ty >= h as i32 {
                        continue;
                    }
                    let ci = sample(tx as u32, ty as u32);
                    if ci != 0 {
                        let c = if eight_bpp {
                            Self::pal256(palette, ci, true)
                        } else {
                            Self::pal16(palette, pal_bank, ci, true)
                        };
                        Self::put_obj(&mut out[x as usize], c, prio, obj_mode, i as u8);
                    }
                }
            } else {
                let hflip = a1 & 0x1000 != 0;
                let vflip = a1 & 0x2000 != 0;
                let ty = if vflip {
                    h - 1 - line as u32
                } else {
                    line as u32
                };
                for dx in 0..w {
                    let x = ox + dx as i32;
                    if !(0..WIDTH as i32).contains(&x) {
                        continue;
                    }
                    let tx = if hflip { w - 1 - dx } else { dx };
                    let ci = sample(tx, ty);
                    if ci != 0 {
                        let c = if eight_bpp {
                            Self::pal256(palette, ci, true)
                        } else {
                            Self::pal16(palette, pal_bank, ci, true)
                        };
                        Self::put_obj(&mut out[x as usize], c, prio, obj_mode, i as u8);
                    }
                }
            }
        }
    }
}

/// Per-frame layer capture for the experimental --3d diorama renderer.
/// Lives outside `Ppu` so save states are untouched; it re-renders the
/// frame's layers from the raw PPU state (registers, palette, VRAM, OAM)
/// once per presented frame. Mid-frame register writes and window/blend
/// effects are ignored, which is fine for a stylized diorama view.
#[derive(Default)]
pub struct Capture {
    /// BG-only composite (all enabled BG layers over the backdrop), ARGB.
    pub bg_frame: Vec<u32>,
    /// Sprite pixels that won compositing: (screen x, screen y, ARGB, OAM
    /// index). The OAM index is what separates two figures standing against
    /// each other: grouping sprite pixels by touching neighbours merged the
    /// player and the NPC beside him into one tall billboard.
    pub sprite_pixels: Vec<(u8, u8, u32, u8)>,
    /// Priority (0-3) of the winning BG layer per pixel; 4 = backdrop.
    pub bg_prio: Vec<u8>,
    /// Index (0-3) of the winning BG layer per pixel; 4 = backdrop.
    pub bg_layer: Vec<u8>,
    /// Color of the winning pixel among BG layers *below* the winner
    /// (the "ground under the overlay"); equals bg_frame where only one
    /// layer is opaque. Used to paint terrain under raised scenery.
    pub bg_under: Vec<u32>,
    /// BG2 (ground layer) scroll registers, for metatile grid alignment.
    pub scroll: (u16, u16),
    /// UI overlay: BG0 pixels that win compositing (dialogue boxes, menus).
    /// 0 = transparent, else 0xFF000000 | color. In FRLG all textbox/menu
    /// UI lives on BG0, which must never extrude into terrain.
    pub ui_frame: Vec<u32>,
    /// True when the picture is not a mode-0 overworld (intro, battles,
    /// affine scenes): the diorama renderer should fall back to flat 2D.
    pub fallback_2d: bool,
    /// Each OAM object's box on screen as [x0, y0, x1, y1] exclusive, from the
    /// OAM attributes alone, so it is NOT clipped to the visible frame; an
    /// empty box means the object is disabled. `sprite_pixels` only records
    /// what actually got drawn, so a character half off the bottom of the
    /// screen measures as a short figure standing further north than he is,
    /// and that measurement slides as the world scrolls under him. The
    /// unclipped box is the fixed thing to measure against.
    pub sprite_boxes: Vec<[i32; 4]>,
}

impl Capture {
    pub fn run(&mut self, io: &[u8], palette: &[u8], vram: &[u8], oam: &[u8]) {
        let r16 = |off: usize| u16::from_le_bytes([io[off], io[off + 1]]);
        let dispcnt = r16(0);
        let backdrop555 = u16::from_le_bytes([palette[0], palette[1]]);
        self.bg_frame.clear();
        self.bg_frame.resize(WIDTH * HEIGHT, rgb555(backdrop555));
        self.bg_prio.clear();
        self.bg_prio.resize(WIDTH * HEIGHT, 4);
        self.bg_layer.clear();
        self.bg_layer.resize(WIDTH * HEIGHT, 4);
        self.bg_under.clear();
        self.bg_under.resize(WIDTH * HEIGHT, rgb555(backdrop555));
        self.sprite_pixels.clear();
        self.sprite_boxes.clear();
        self.sprite_boxes.resize(128, [0; 4]);
        for i in 0..128 {
            let a0 = u16::from_le_bytes([oam[i * 8], oam[i * 8 + 1]]) as u32;
            let a1 = u16::from_le_bytes([oam[i * 8 + 2], oam[i * 8 + 3]]) as u32;
            let affine = a0 & 0x100 != 0;
            if (!affine && a0 & 0x200 != 0) || a0 >> 10 & 3 == 3 {
                continue;
            }
            let (w, h): (u32, u32) = match (a0 >> 14 & 3, a1 >> 14 & 3) {
                (0, s) => (8 << s, 8 << s),
                (1, 0) => (16, 8),
                (1, 1) => (32, 8),
                (1, 2) => (32, 16),
                (1, 3) => (64, 32),
                (2, 0) => (8, 16),
                (2, 1) => (8, 32),
                (2, 2) => (16, 32),
                _ => (32, 64),
            };
            let (bw, bh) = if affine && a0 & 0x200 != 0 { (w * 2, h * 2) } else { (w, h) };
            let (sy, sx) = (a0 & 0xFF, a1 & 0x1FF);
            let oy = if sy + bh > 256 { sy as i32 - 256 } else { sy as i32 };
            let ox = if sx + bw > 512 { sx as i32 - 512 } else { sx as i32 };
            self.sprite_boxes[i] = [ox, oy, ox + bw as i32, oy + bh as i32];
        }
        self.ui_frame.clear();
        self.ui_frame.resize(WIDTH * HEIGHT, 0);
        self.scroll = (r16(0x18) & 0x1FF, r16(0x1A) & 0x1FF);
        self.fallback_2d = dispcnt & 7 != 0;
        if dispcnt & 0x80 != 0 {
            return; // forced blank
        }
        // Affine reference points, reloaded from the registers and stepped
        // by PB/PD per line (mid-frame reloads are not modeled).
        let mut bg_ref = [(0i32, 0i32); 2];
        for (bg, r) in bg_ref.iter_mut().enumerate() {
            let base = 0x28 + bg * 0x10;
            let rd = |o: usize| {
                let v = u32::from_le_bytes([io[o], io[o + 1], io[o + 2], io[o + 3]]);
                ((v << 4) as i32) >> 4
            };
            *r = (rd(base), rd(base + 4));
        }
        for y in 0..HEIGHT as u32 {
            let mut bg_line = [[0u16; WIDTH]; 4];
            let mut obj_line = [ObjPixel::default(); WIDTH];
            Ppu::draw_bg_layers(y, dispcnt, io, palette, vram, &bg_ref, &mut bg_line);
            if dispcnt & 0x1000 != 0 {
                Ppu::render_sprites(y, dispcnt, palette, vram, oam, &mut obj_line);
            }
            for (bg, r) in bg_ref.iter_mut().enumerate() {
                let base = 0x22 + bg * 0x10;
                r.0 += r16(base) as i16 as i32;
                r.1 += r16(base + 4) as i16 as i32;
            }
            let row = y as usize * WIDTH;
            for x in 0..WIDTH {
                // Top BG pixel by priority, same search order as compositing.
                // BG0 is the UI layer (dialogue, menus): captured separately
                // as a flat overlay, never as terrain.
                let mut bg_top: Option<(u16, u8, u8)> = None;
                let mut bg_under: Option<u16> = None;
                let mut ui: Option<u8> = None;
                'bg: for prio in 0..4u8 {
                    for bg in 0..4usize {
                        if dispcnt & (1 << (8 + bg)) == 0
                            || (r16(0x8 + bg * 2) & 3) as u8 != prio
                        {
                            continue;
                        }
                        let p = bg_line[bg][x];
                        if p & 0x8000 == 0 {
                            continue;
                        }
                        if bg == 0 {
                            if ui.is_none() && bg_top.is_none() {
                                ui = Some(prio);
                                self.ui_frame[row + x] = 0xFF00_0000 | rgb555(p & 0x7FFF);
                            }
                        } else if bg_top.is_none() {
                            bg_top = Some((p & 0x7FFF, prio, bg as u8));
                        } else {
                            bg_under = Some(p & 0x7FFF);
                            break 'bg;
                        }
                    }
                }
                if let Some((c, prio, layer)) = bg_top {
                    self.bg_frame[row + x] = rgb555(c);
                    self.bg_prio[row + x] = prio;
                    self.bg_layer[row + x] = layer;
                    self.bg_under[row + x] = rgb555(bg_under.unwrap_or(c));
                }
                let obj = &obj_line[x];
                if obj.opaque
                    && obj.prio <= bg_top.map_or(4, |(_, p, _)| p)
                    && ui.is_none_or(|up| obj.prio <= up)
                {
                    if ui.is_some() {
                        // A sprite drawn over a BG0 window is UI content, not
                        // a figure in the world: the Pokemon portrait in the
                        // starter-choice popup, cursors, party icons. Those
                        // must composite flat with the window they sit in,
                        // otherwise the window draws as an empty white box and
                        // the portrait is lost somewhere in the diorama.
                        self.ui_frame[row + x] = 0xFF00_0000 | rgb555(obj.color);
                    } else {
                        self.sprite_pixels
                            .push((x as u8, y as u8, rgb555(obj.color), obj.obj));
                    }
                }
            }
        }
    }
}
