//! Experimental "voxel diorama" renderer (--3d flag, native frontend only).
//!
//! The world is built from the GAME'S MAP, not from the screen. Every frame we
//! read FireRed's live map grid out of emulated RAM, decode the metatile
//! artwork straight from the tileset definitions in ROM plus the tile graphics
//! the game already unpacked into VRAM, and emit geometry in MAP coordinates.
//!
//! That distinction is the whole point of this file. An earlier version
//! rebuilt the diorama every frame out of the 240x160 pixels on screen, which
//! made every height a function of where a thing happened to be on screen:
//! scenery had to grow out of the ground as it scrolled in (the "buildings
//! twitch while walking" bug), and a one-cell signpost became a little cube
//! wearing its own art as a roof. With map-space geometry a building is the
//! same solid object at the same height on every frame, the world simply
//! extends past the visible frame, and there is no edge ramp to speak of.
//!
//! Sprites still come from the captured PPU pixels (they are drawings, not map
//! data) and are drawn as flat feet-pivoted billboards leaning back by the
//! camera pitch, with contact shadows and an occlusion silhouette. BG0 UI
//! composites flat on top. Everything is software-rasterized (z-buffered).
use gba::bus::Bus;
use gba::ppu::{self, Capture};

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 500;

/// Camera pitch down from horizontal: 35 degrees (the reference mod's
/// default diorama tilt).
const SIN_P: f32 = 0.573_576_4;
const COS_P: f32 = 0.819_152;
/// Camera distance from the diorama center (world units = GBA pixels).
const CAM_DIST: f32 = 340.0;
/// Focal length in screen pixels.
const FOCAL: f32 = 940.0;
const CX: f32 = WIDTH as f32 / 2.0;
const CY: f32 = 236.0;

const BACKGROUND_TOP: u32 = 0x0016203A;
const BACKGROUND_BOT: u32 = 0x00060A14;

/// Sentinel a texture sampler returns for a transparent texel: the pixel is
/// skipped entirely (no color, no depth), cutting the silhouette out of the
/// quad.
const SKIP: u32 = 0xFFFF_FFFF;

/// Height of one metatile step, in world units (= GBA pixels).
const STEP: f32 = 16.0;
/// Water sits below ground so shorelines get a lip.
const WATER: f32 = -3.0;

#[inline]
fn rgb555(c: u16) -> u32 {
    let r = (c & 0x1F) as u32;
    let g = (c >> 5 & 0x1F) as u32;
    let b = (c >> 10 & 0x1F) as u32;
    (r << 19 | r >> 2 << 16) | (g << 11 | g >> 2 << 8) | (b << 3 | b >> 2)
}

#[inline]
fn shade(color: u32, f: f32) -> u32 {
    let ch = |c: u32| ((c as f32 * f).min(255.0)) as u32;
    ch(color >> 16 & 0xFF) << 16 | ch(color >> 8 & 0xFF) << 8 | ch(color & 0xFF)
}

/// Project a world point (x right, y up, z away from camera into the scene)
/// to (screen x, screen y, view depth).
#[inline]
fn project(wx: f32, wy: f32, wz: f32) -> (f32, f32, f32) {
    let yv = wy * COS_P + wz * SIN_P;
    let zv = CAM_DIST + wz * COS_P - wy * SIN_P;
    (CX + FOCAL * wx / zv, CY - FOCAL * yv / zv, zv)
}

/// Map a capture pixel (px, py) plus height to world space: the visible GBA
/// screen lies centered in the ground plane, top of the screen farthest away.
/// Used for sprites, which are still screen-space drawings.
#[inline]
fn world(px: f32, py: f32, h: f32) -> (f32, f32, f32) {
    (px - ppu::WIDTH as f32 / 2.0, h, ppu::HEIGHT as f32 / 2.0 - py)
}

// ---------------------------------------------------------------------------
// Metatile artwork
// ---------------------------------------------------------------------------

/// Decoded 16x16 artwork for one metatile id, cached by id for the frame.
///
/// FireRed metatiles are eight 8x8 tile references: four for the bottom layer
/// (the ground) and four for the top layer (whatever is drawn over it). We
/// keep both the composite (what the player sees) and the top layer alone,
/// because the top layer alone IS the prop: a signpost's art without the grass
/// it stands on, which is exactly what a flat billboard wants.
pub struct Art {
    /// Composite pixels, 256 per decoded metatile.
    comp: Vec<u32>,
    /// Top layer only; SKIP where transparent.
    top: Vec<u32>,
    /// Flat material color (average of the composite), for unpainted faces.
    flat: Vec<u32>,
    /// True when the composite is essentially all black: FireRed pads the
    /// space around interiors with black metatiles that are inside the map and
    /// drawn by a real layer, and a diorama must cut those out rather than lay
    /// down a black floor slab.
    dark: Vec<bool>,
    /// Metatile id -> slot index, or -1 when not decoded yet.
    slot: Vec<i32>,
}

impl Art {
    fn new() -> Art {
        Art {
            comp: Vec::new(),
            top: Vec::new(),
            flat: Vec::new(),
            dark: Vec::new(),
            slot: vec![-1; 1024],
        }
    }

    #[inline]
    fn comp_at(&self, slot: usize, x: usize, y: usize) -> u32 {
        self.comp[slot * 256 + y * 16 + x]
    }

    #[inline]
    fn top_at(&self, slot: usize, x: usize, y: usize) -> u32 {
        self.top[slot * 256 + y * 16 + x]
    }
}

/// Everything needed to turn a metatile id into pixels: where the metatile
/// definitions live in ROM, where the game unpacked the tile graphics in VRAM,
/// and the live BG palettes.
struct ArtSource<'a> {
    rom: &'a [u8],
    vram: &'a [u8],
    palette: &'a [u8],
    /// Metatile definition tables for the primary and secondary tilesets.
    meta: (u32, u32),
    /// VRAM char block base the map layers render from.
    char_base: usize,
}

impl ArtSource<'_> {
    fn rd16(&self, a: u32) -> u16 {
        match a >> 24 {
            0x08..=0x09 => {
                let i = (a as usize).wrapping_sub(0x0800_0000);
                if i + 1 < self.rom.len() {
                    u16::from_le_bytes([self.rom[i], self.rom[i + 1]])
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// One 8x8 4bpp tile from VRAM into `out` at (ox, oy) of a 16x16 block.
    /// `entry` is a metatile tile reference: id 0-9, hflip 10, vflip 11,
    /// palette 12-15. `layer_top` leaves index 0 transparent (SKIP).
    fn blit_tile(&self, entry: u16, out: &mut [u32], ox: usize, oy: usize, layer_top: bool) {
        let tile = (entry & 0x3FF) as usize;
        let (hf, vf) = (entry & 0x400 != 0, entry & 0x800 != 0);
        let pal = (entry >> 12) as usize * 32;
        let base = self.char_base + tile * 32;
        if base + 32 > self.vram.len() || pal + 32 > self.palette.len() {
            return;
        }
        for row in 0..8 {
            let sy = if vf { 7 - row } else { row };
            for col in 0..8 {
                let sx = if hf { 7 - col } else { col };
                let b = self.vram[base + sy * 4 + sx / 2];
                let idx = if sx & 1 == 0 { b & 0xF } else { b >> 4 } as usize;
                let c = if idx == 0 {
                    if layer_top {
                        continue; // transparent: leave whatever is underneath
                    }
                    rgb555(u16::from_le_bytes([self.palette[0], self.palette[1]]))
                } else {
                    rgb555(u16::from_le_bytes([
                        self.palette[pal + idx * 2],
                        self.palette[pal + idx * 2 + 1],
                    ]))
                };
                out[(oy + row) * 16 + ox + col] = c;
            }
        }
    }

    /// Decode metatile `id` into `art`, returning its slot.
    fn decode(&self, art: &mut Art, id: u16) -> usize {
        let id = (id & 0x3FF) as usize;
        if art.slot[id] >= 0 {
            return art.slot[id] as usize;
        }
        let slot = art.comp.len() / 256;
        art.slot[id] = slot as i32;
        // Metatile definition: 8 u16 entries. Ids below 0x280 come from the
        // primary tileset, the rest from the secondary one.
        let def = if id < 0x280 {
            self.meta.0 + id as u32 * 16
        } else {
            self.meta.1 + (id as u32 - 0x280) * 16
        };
        let mut comp = [0u32; 256];
        let mut top = [SKIP; 256];
        for (quad, out_top) in [(0u32, false), (4, true)] {
            for q in 0..4u32 {
                let e = self.rd16(def + (quad + q) * 2);
                let (ox, oy) = ((q as usize & 1) * 8, (q as usize / 2) * 8);
                self.blit_tile(e, &mut comp, ox, oy, out_top);
                if out_top {
                    self.blit_tile(e, &mut top, ox, oy, true);
                }
            }
        }
        // Flat material color and the all-black test.
        let (mut r, mut g, mut b, mut lit) = (0u32, 0u32, 0u32, 0u32);
        for &c in comp.iter() {
            r += c >> 16 & 0xFF;
            g += c >> 8 & 0xFF;
            b += c & 0xFF;
            if (c >> 16 & 0xFF).max(c >> 8 & 0xFF).max(c & 0xFF) > 24 {
                lit += 1;
            }
        }
        art.comp.extend_from_slice(&comp);
        art.top.extend_from_slice(&top);
        art.flat.push((r / 256) << 16 | (g / 256) << 8 | (b / 256));
        art.dark.push(lit * 8 < 256);
        slot
    }
}

// ---------------------------------------------------------------------------
// Map grid
// ---------------------------------------------------------------------------

/// One map cell, classified from the game's own collision and behavior data.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cell {
    Flat,
    Grass, // MB_TALL_GRASS etc: a walk-through overlay, never geometry
    Water, // flat, slightly sunken so shorelines get a lip
    /// Nothing here: outside the real map, or one of FireRed's black padding
    /// metatiles. Never drawn, so the background shows through.
    Void,
    /// A one-cell solid: signpost, mailbox, small rock. Rendered as the ground
    /// underneath plus a flat upright billboard of the metatile's TOP layer.
    /// Never an extruded cube wearing its own art as a roof.
    Prop,
    /// Part of a solid volume standing on this cell.
    ///
    /// `h` world height, `n` cells the volume spans north-to-south, `k` this
    /// cell's index from the volume's north edge, `wall` how many of the
    /// volume's southernmost cells are its drawn front (0 = the art has no
    /// front, so the face is painted flat instead of folded).
    Block { h: f32, n: u8, k: u8, wall: u8 },
}

/// A window of FireRed's live map grid around the player, in MAP coordinates,
/// with the metatile artwork it needs. The window extends well past the
/// visible GBA frame, so nothing has to grow into existence at the edge.
pub struct MapGrid {
    /// Fine scroll of the ground layer, 0..15 pixels (sub-cell camera motion).
    pub fine: (usize, usize),
    pub cells: Vec<Cell>,
    /// Art slot per cell.
    slots: Vec<u32>,
    /// Height per cell (ground 0, water sunken, volumes extruded).
    height: Vec<f32>,
    art: Art,
    /// World x of the west edge of column 0, world z of the north edge of
    /// row 0. Both move continuously with the camera.
    ox: f32,
    oz: f32,
}

/// Cells of margin kept outside the visible frame on each side. North needs
/// the most: the camera tilt makes distant rows climb the screen.
const MX: i32 = 8;
const MY_N: i32 = 10;
const MY_S: i32 = 3;

impl MapGrid {
    /// Columns and rows in the window (the visible frame is 16 x 11 of them).
    pub const COLS: usize = (16 + 2 * MX) as usize;
    pub const ROWS: usize = (11 + MY_N + MY_S) as usize;

    #[inline]
    fn at(&self, cx: i32, cy: i32) -> Cell {
        if cx < 0 || cy < 0 || cx >= Self::COLS as i32 || cy >= Self::ROWS as i32 {
            return Cell::Void;
        }
        self.cells[cy as usize * Self::COLS + cx as usize]
    }

    /// Height a neighbouring cell presents to a face. Void reads as open
    /// ground so a volume beside a cut-out still shows its wall.
    #[inline]
    fn neighbor_h(&self, cx: i32, cy: i32) -> f32 {
        if cx < 0 || cy < 0 || cx >= Self::COLS as i32 || cy >= Self::ROWS as i32 {
            return 0.0;
        }
        match self.cells[cy as usize * Self::COLS + cx as usize] {
            Cell::Void => 0.0,
            _ => self.height[cy as usize * Self::COLS + cx as usize],
        }
    }

    /// FireRed (US Rev 1) addresses: gBackupMapLayout (width, height, grid
    /// pointer; the grid is the LIVE map with a 7-cell border margin),
    /// gMapHeader (-> ROM map layout -> tilesets -> metatiles + attributes),
    /// gSaveBlock1Ptr (player map coordinates = camera).
    pub fn read(bus: &Bus) -> Option<MapGrid> {
        let rd8 = |a: u32| -> Option<u8> {
            match a >> 24 {
                0x02 => bus.ewram.get((a as usize) & 0x3_FFFF).copied(),
                0x03 => bus.iwram.get((a as usize) & 0x7FFF).copied(),
                0x08..=0x09 => bus.rom.get((a as usize) - 0x0800_0000).copied(),
                _ => None,
            }
        };
        let rd16 = |a: u32| Some(u16::from_le_bytes([rd8(a)?, rd8(a + 1)?]));
        let rd32 =
            |a: u32| Some(u32::from_le_bytes([rd8(a)?, rd8(a + 1)?, rd8(a + 2)?, rd8(a + 3)?]));

        let vwidth = rd32(0x0300_5040)? as i32; // map width + 15 border cells
        let vheight = rd32(0x0300_5044)? as i32;
        let grid_ptr = rd32(0x0300_5048)?;
        if !(1..=1024).contains(&vwidth) || !(1..=1024).contains(&vheight) || grid_ptr >> 24 != 2 {
            return None;
        }
        // The live grid carries a 7-cell margin on each side of the real map.
        // That margin is not junk: it holds the border metatiles the game
        // shows past a map edge, and the neighbouring map's cells wherever a
        // connection exists. We render all of it, so walking to the southern
        // edge of Viridian shows Route 1 coming up rather than a cliff into
        // the void.
        if vwidth <= 15 || vheight <= 15 {
            return None;
        }
        let sb1 = rd32(0x0300_5008)?;
        if sb1 >> 24 != 2 {
            return None;
        }
        let px = rd16(sb1)? as i16 as i32;
        let py = rd16(sb1 + 2)? as i16 as i32;
        let layout = rd32(0x0203_6DFC)?;
        // Tileset struct in FRLG: +0x00 isCompressed, +0x01 isSecondary,
        // +0x04 tiles, +0x08 palettes, +0x0C metatiles, +0x10 init callback,
        // +0x14 metatile attributes. The callback at +0x10 is what the
        // previous version mistook for the attribute table, which is why
        // water and tall grass used to be classified from garbage.
        let tileset = |slot: u32| -> Option<u32> {
            let t = rd32(slot)?;
            (t >> 24 == 8 || t >> 24 == 9).then_some(t)
        };
        let dbg = std::env::var("GBA_3D_DEBUG").is_ok();
        if dbg {
            eprintln!("grid: vw={vwidth} vh={vheight} grid={grid_ptr:08X} sb1={sb1:08X} p=({px},{py}) layout={layout:08X}");
        }
        let (tprim, tsec) = (tileset(layout + 0x10)?, tileset(layout + 0x14)?);
        let field = |t: u32, off: u32| -> Option<u32> {
            let p = rd32(t + off)?;
            (p >> 24 == 8 || p >> 24 == 9).then_some(p)
        };
        let (aprim, asec) = (field(tprim, 0x14)?, field(tsec, 0x14)?);
        let (mprim, msec) = (field(tprim, 0x0C)?, field(tsec, 0x0C)?);
        if dbg {
            eprintln!("grid: tsets {tprim:08X}/{tsec:08X} meta {mprim:08X}/{msec:08X} attr {aprim:08X}/{asec:08X}");
        }

        // Live grid entry: metatile id 0-9, collision 10-11, elevation 12-15.
        // VMap coordinates are map coordinates + 7 (the border margin).
        let entry = |gx: i32, gy: i32| -> Option<u16> {
            let (vx, vy) = (gx + 7, gy + 7);
            if vx < 0 || vy < 0 || vx >= vwidth || vy >= vheight {
                return None; // past the live grid entirely: nothing to draw
            }
            // Metatile id 0x3FF is FireRed's MAPGRID_UNDEFINED: the parts of
            // the margin with no map connection behind them. The game never
            // draws those, and decoding one reads past the metatile table and
            // produces garbage.
            rd16(grid_ptr + ((vy * vwidth + vx) * 2) as u32).filter(|&e| e & 0x3FF != 0x3FF)
        };
        let behavior = |e: u16| -> u32 {
            let m = (e & 0x3FF) as u32;
            let a = if m < 0x280 { rd32(aprim + m * 4) } else { rd32(asec + (m - 0x280) * 4) };
            a.unwrap_or(0) & 0x1FF
        };
        let is_water = |e: u16| (0x10..=0x2F).contains(&behavior(e));
        // A cell is solid geometry when the game blocks movement into it and
        // it is not water (water is blocked too, until you have Surf).
        let solid = |gx: i32, gy: i32| -> bool {
            entry(gx, gy).is_some_and(|e| e >> 10 & 3 != 0 && !is_water(e))
        };

        // Screen top-left cell: the player cell is centered at screen cell
        // (7, 5); fine scroll from the ground layer's BG registers.
        let r16io = |off: usize| u16::from_le_bytes([bus.io[off], bus.io[off + 1]]);
        let (hofs, vofs) = (r16io(0x18) & 0x1FF, r16io(0x1A) & 0x1FF);
        let fine = ((hofs % 16) as usize, (vofs % 16) as usize);
        let (gx0, gy0) = (px - 7 - MX, py - 5 - MY_N);

        let src = ArtSource {
            rom: &bus.rom,
            vram: &bus.vram,
            palette: &bus.palette,
            meta: (mprim, msec),
            // The map layers all share one char block; take it from the
            // ground layer's control register rather than assuming zero.
            char_base: ((r16io(0x0C) >> 2) & 3) as usize * 0x4000,
        };
        let mut art = Art::new();

        let mut cells = Vec::with_capacity(Self::COLS * Self::ROWS);
        let mut slots = Vec::with_capacity(Self::COLS * Self::ROWS);
        let mut height = Vec::with_capacity(Self::COLS * Self::ROWS);
        for cy in 0..Self::ROWS as i32 {
            for cx in 0..Self::COLS as i32 {
                let (gx, gy) = (gx0 + cx, gy0 + cy);
                let Some(e) = entry(gx, gy) else {
                    cells.push(Cell::Void);
                    slots.push(0);
                    height.push(0.0);
                    continue;
                };
                let slot = src.decode(&mut art, e) as u32;
                // Black padding metatiles are not scenery, they are the void
                // the game hides off the edge of a 240x160 screen.
                if art.dark[slot as usize] {
                    cells.push(Cell::Void);
                    slots.push(slot);
                    height.push(0.0);
                    continue;
                }
                let cell = if solid(gx, gy) {
                    Self::classify_volume(gx, gy, &solid, &entry)
                } else if is_water(e) {
                    Cell::Water
                } else {
                    match behavior(e) {
                        0x02 | 0x03 => Cell::Grass,
                        _ => Cell::Flat,
                    }
                };
                height.push(match cell {
                    Cell::Water => WATER,
                    Cell::Block { h, .. } => h,
                    _ => 0.0,
                });
                cells.push(cell);
                slots.push(slot);
            }
        }
        Some(MapGrid {
            fine,
            cells,
            slots,
            height,
            art,
            ox: -(MX as f32) * STEP - fine.0 as f32 - ppu::WIDTH as f32 / 2.0,
            oz: ppu::HEIGHT as f32 / 2.0 + MY_N as f32 * STEP + fine.1 as f32,
        })
    }

    /// Decide what solid volume the cell at (gx, gy) belongs to, purely from
    /// map data, so the answer is the same on every frame no matter where the
    /// cell sits on screen.
    ///
    /// A vertical run of solid cells in top-down art is one object seen from
    /// above: its southern rows are the front the player sees (a house wall
    /// with a door, a tree trunk) and its northern rows are the roof. We split
    /// the run accordingly:
    ///   * a run of one cell is a prop (signpost, mailbox) and stays flat;
    ///   * a run of identical metatiles is a repeating row (a hedge, a cliff
    ///     face), so every cell is its own one-step block wearing its own art;
    ///   * a run that repeats with period two is trees: canopy over trunk, so
    ///     each pair is a block one step tall with the trunk as its front;
    ///   * anything else is one object, its bottom rows (up to two) the front,
    ///     the rest the roof.
    fn classify_volume(
        gx: i32,
        gy: i32,
        solid: &impl Fn(i32, i32) -> bool,
        entry: &impl Fn(i32, i32) -> Option<u16>,
    ) -> Cell {
        const MAX: i32 = 8;
        let mut top = gy;
        while gy - top < MAX && solid(gx, top - 1) {
            top -= 1;
        }
        let mut bot = gy;
        while bot - gy < MAX && solid(gx, bot + 1) {
            bot += 1;
        }
        let len = (bot - top + 1) as usize;
        let j = (gy - top) as usize; // index from the run's north end
        let id = |i: usize| entry(gx, top + i as i32).unwrap_or(0) & 0x3FF;
        if len == 1 {
            return Cell::Prop;
        }
        let period = |p: usize| len > p && (0..len - p).all(|i| id(i) == id(i + p));
        if period(1) {
            return Cell::Block { h: STEP, n: 1, k: 0, wall: 0 };
        }
        if period(2) {
            // Pairs anchored at the run's north end: canopy, trunk, canopy...
            let k = (j % 2) as u8;
            // A trailing odd cell has no partner; treat it as its own block.
            let paired = j / 2 * 2 + 1 < len;
            if paired {
                return Cell::Block { h: STEP, n: 2, k, wall: 1 };
            }
            return Cell::Block { h: STEP, n: 1, k: 0, wall: 0 };
        }
        let wall = (len - 1).min(2) as u8;
        Cell::Block { h: STEP * wall as f32, n: len as u8, k: j as u8, wall }
    }

    /// Screen pixel -> window cell, for anchoring sprites and for the
    /// GBA_GRID_DEBUG alignment overlay.
    pub fn cell_of_screen(&self, px: usize, py: usize) -> Cell {
        let cx = ((px + self.fine.0) / 16) as i32 + MX;
        let cy = ((py + self.fine.1) / 16) as i32 + MY_N;
        self.at(cx, cy)
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

pub struct Renderer {
    pub buffer: Vec<u32>,
    zbuf: Vec<f32>,
    background: Vec<u32>,
    /// Tilt-shift level 0-3 from GBA_TILT (default 2; 0 = off).
    tilt: u32,
    /// Last known state of the map-grid read, so the fallback logs once on
    /// each transition instead of every frame.
    grid_ok: bool,
    no_walls: bool,
}

impl Renderer {
    pub fn new() -> Self {
        // Background: subtle vertical gradient, precomputed once.
        let mut background = vec![0u32; WIDTH * HEIGHT];
        for y in 0..HEIGHT {
            let t = y as f32 / HEIGHT as f32;
            let lerp = |a: u32, b: u32, s: u32| {
                let (a, b) = (a >> s & 0xFF, b >> s & 0xFF);
                ((a as f32 + (b as f32 - a as f32) * t) as u32) << s
            };
            let c = lerp(BACKGROUND_TOP, BACKGROUND_BOT, 16)
                | lerp(BACKGROUND_TOP, BACKGROUND_BOT, 8)
                | lerp(BACKGROUND_TOP, BACKGROUND_BOT, 0);
            background[y * WIDTH..(y + 1) * WIDTH].fill(c);
        }
        Self {
            buffer: background.clone(),
            zbuf: vec![f32::INFINITY; WIDTH * HEIGHT],
            background,
            tilt: std::env::var("GBA_TILT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2)
                .min(3),
            grid_ok: true,
            no_walls: std::env::var("GBA_NO_WALLS").is_ok(),
        }
    }

    /// Z-buffered flat triangle. `dim`: None = solid `color`, Some(f) =
    /// multiply the destination by f (used for soft shadows).
    fn tri(&mut self, p: [(f32, f32, f32); 3], color: u32, dim: Option<f32>) {
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
        if area.abs() < 1e-6 {
            return;
        }
        let min_x = (p[0].0.min(p[1].0).min(p[2].0).floor().max(0.0)) as usize;
        let max_x = (p[0].0.max(p[1].0).max(p[2].0).ceil()).min(WIDTH as f32 - 1.0) as usize;
        let min_y = (p[0].1.min(p[1].1).min(p[2].1).floor().max(0.0)) as usize;
        let max_y = (p[0].1.max(p[1].1).max(p[2].1).ceil()).min(HEIGHT as f32 - 1.0) as usize;
        let inv = 1.0 / area;
        for y in min_y..=max_y {
            let fy = y as f32 + 0.5;
            for x in min_x..=max_x {
                let fx = x as f32 + 0.5;
                let w0 = ((p[1].0 - fx) * (p[2].1 - fy) - (p[1].1 - fy) * (p[2].0 - fx)) * inv;
                let w1 = ((p[2].0 - fx) * (p[0].1 - fy) - (p[2].1 - fy) * (p[0].0 - fx)) * inv;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let z = w0 * p[0].2 + w1 * p[1].2 + w2 * p[2].2;
                let i = y * WIDTH + x;
                if z < self.zbuf[i] {
                    match dim {
                        // Shadows land on geometry only: an untouched depth
                        // slot is exposed background, and darkening the sky
                        // under a figure standing at a map edge reads as a
                        // smudge floating in the void.
                        Some(_) if !self.zbuf[i].is_finite() => {}
                        Some(f) => self.buffer[i] = shade(self.buffer[i], f),
                        None => {
                            self.zbuf[i] = z;
                            self.buffer[i] = color;
                        }
                    }
                }
            }
        }
    }

    /// Z-buffered textured triangle: uv interpolated barycentrically,
    /// color from `sample(u, v)`.
    fn tri_uv(
        &mut self,
        p: [(f32, f32, f32); 3],
        uv: [(f32, f32); 3],
        sample: &mut impl FnMut(f32, f32) -> u32,
    ) {
        self.tri_uv_mode(p, uv, sample, false)
    }

    /// `ghost`: inverted depth test, no depth write, dimmed color -- used to
    /// repaint the player's silhouette where scenery hides it (the
    /// reference mod's occlusion silhouette).
    fn tri_uv_mode(
        &mut self,
        p: [(f32, f32, f32); 3],
        uv: [(f32, f32); 3],
        sample: &mut impl FnMut(f32, f32) -> u32,
        ghost: bool,
    ) {
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
        if area.abs() < 1e-6 {
            return;
        }
        if p[0].0.max(p[1].0).max(p[2].0) < 0.0
            || p[0].0.min(p[1].0).min(p[2].0) > WIDTH as f32
            || p[0].1.max(p[1].1).max(p[2].1) < 0.0
            || p[0].1.min(p[1].1).min(p[2].1) > HEIGHT as f32
        {
            return; // fully offscreen: the world is bigger than the frame
        }
        let min_x = (p[0].0.min(p[1].0).min(p[2].0).floor().max(0.0)) as usize;
        let max_x = (p[0].0.max(p[1].0).max(p[2].0).ceil()).min(WIDTH as f32 - 1.0) as usize;
        let min_y = (p[0].1.min(p[1].1).min(p[2].1).floor().max(0.0)) as usize;
        let max_y = (p[0].1.max(p[1].1).max(p[2].1).ceil()).min(HEIGHT as f32 - 1.0) as usize;
        let inv = 1.0 / area;
        for y in min_y..=max_y {
            let fy = y as f32 + 0.5;
            for x in min_x..=max_x {
                let fx = x as f32 + 0.5;
                let w0 = ((p[1].0 - fx) * (p[2].1 - fy) - (p[1].1 - fy) * (p[2].0 - fx)) * inv;
                let w1 = ((p[2].0 - fx) * (p[0].1 - fy) - (p[2].1 - fy) * (p[0].0 - fx)) * inv;
                let w2 = 1.0 - w0 - w1;
                // Slightly tolerant edge test so abutting quads never leave
                // one-pixel seams between their coverage regions.
                const E: f32 = -0.002;
                if w0 < E || w1 < E || w2 < E {
                    continue;
                }
                let z = w0 * p[0].2 + w1 * p[1].2 + w2 * p[2].2;
                let i = y * WIDTH + x;
                if (z < self.zbuf[i]) != ghost {
                    let u = w0 * uv[0].0 + w1 * uv[1].0 + w2 * uv[2].0;
                    let v = w0 * uv[0].1 + w1 * uv[1].1 + w2 * uv[2].1;
                    let c = sample(u, v);
                    if c != SKIP {
                        if ghost {
                            // Translucent silhouette: blend toward the scene.
                            let mix = |s: u32, a: u32| {
                                (((a >> s & 0xFF) + (self.buffer[i] >> s & 0xFF) * 2) / 3) << s
                            };
                            self.buffer[i] = mix(16, c) | mix(8, c) | mix(0, c);
                        } else {
                            self.zbuf[i] = z;
                            self.buffer[i] = c;
                        }
                    }
                }
            }
        }
    }

    /// Textured quad: vertices clockwise from top-left, uv (0,0) at q[0],
    /// (1,0) at q[1], (1,1) at q[2], (0,1) at q[3].
    fn quad_uv(&mut self, q: [(f32, f32, f32); 4], sample: &mut impl FnMut(f32, f32) -> u32) {
        self.tri_uv([q[0], q[1], q[2]], [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], sample);
        self.tri_uv([q[0], q[2], q[3]], [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0)], sample);
    }

    fn quad_uv_ghost(&mut self, q: [(f32, f32, f32); 4], sample: &mut impl FnMut(f32, f32) -> u32) {
        self.tri_uv_mode([q[0], q[1], q[2]], [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], sample, true);
        self.tri_uv_mode([q[0], q[2], q[3]], [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0)], sample, true);
    }

    /// Blit a 240x160 frame at 3x nearest-neighbor, centered. `opaque_only`
    /// skips zero (transparent) pixels — used for the flat UI overlay.
    fn blit_2d(&mut self, frame: &[u32], opaque_only: bool) {
        const X0: usize = (WIDTH - ppu::WIDTH * 3) / 2;
        const Y0: usize = (HEIGHT - ppu::HEIGHT * 3) / 2;
        for y in 0..ppu::HEIGHT {
            for x in 0..ppu::WIDTH {
                let c = frame[y * ppu::WIDTH + x];
                if opaque_only && c & 0xFF00_0000 == 0 {
                    continue;
                }
                let c = c & 0x00FF_FFFF;
                for dy in 0..3 {
                    let row = (Y0 + y * 3 + dy) * WIDTH + X0 + x * 3;
                    self.buffer[row..row + 3].fill(c);
                }
            }
        }
    }

    /// Draw one diorama frame. `flat` is the PPU's real 2D framebuffer, used
    /// verbatim when the scene is not a mode-0 overworld (intro, battles) or
    /// when UI covers most of the picture.
    pub fn render(&mut self, cap: &Capture, flat: &[u32], grid: Option<&MapGrid>) {
        self.buffer.copy_from_slice(&self.background);
        self.zbuf.fill(f32::INFINITY);
        if cap.bg_frame.len() < ppu::WIDTH * ppu::HEIGHT {
            return; // no capture yet (first frame)
        }
        let ui_pixels = cap.ui_frame.iter().filter(|&&c| c != 0).count();
        if cap.fallback_2d || ui_pixels > ppu::WIDTH * ppu::HEIGHT / 2 {
            self.blit_2d(flat, false);
            return;
        }
        // No live map grid means this is not an overworld: a battle, a
        // cutscene, the intro. There is no world to build, so show the real
        // 2D picture.
        let Some(g) = grid else {
            if self.grid_ok {
                self.grid_ok = false;
                eprintln!("3d: no map grid, showing 2D");
            }
            self.blit_2d(flat, false);
            return;
        };
        if !self.grid_ok {
            self.grid_ok = true;
            eprintln!("3d: map grid back");
        }

        self.render_world(g);
        self.render_sprites(cap, g);
        if self.tilt > 0 {
            self.tilt_shift(self.tilt);
        }
        // Dialogue boxes and menus (BG0) composite flat in screen space on
        // top of the finished 3D scene — UI must never extrude.
        if ui_pixels > 0 {
            self.blit_2d(&cap.ui_frame, true);
        }
    }

    /// The diorama itself: one pass over the map window emitting map-space
    /// geometry. Nothing here looks at where a cell falls on the GBA screen.
    fn render_world(&mut self, g: &MapGrid) {
        // Near rows first. Everything is opaque and z-buffered, so drawing
        // front-to-back lets the depth test reject hidden cells before their
        // texture sampler ever runs, which is most of the cost.
        for cy in (0..MapGrid::ROWS as i32).rev() {
            for cx in 0..MapGrid::COLS as i32 {
                let i = cy as usize * MapGrid::COLS + cx as usize;
                let cell = g.cells[i];
                if cell == Cell::Void {
                    continue;
                }
                let (x0, x1) = (g.ox + cx as f32 * STEP, g.ox + (cx + 1) as f32 * STEP);
                let (zn, zs) = (g.oz - cy as f32 * STEP, g.oz - (cy + 1) as f32 * STEP);
                let h = g.height[i];
                let slot = g.slots[i] as usize;

                // Top face. For a volume the roof art is whatever of the
                // object's rows is not spent on its front, stretched over the
                // whole footprint; for ground it is simply the cell's art.
                let (n, k, wall) = match cell {
                    Cell::Block { n, k, wall, .. } => (n as i32, k as i32, wall as i32),
                    _ => (1, 0, 0),
                };
                let roof = (n - wall).max(1);
                let top = [
                    project(x0, h, zn),
                    project(x1, h, zn),
                    project(x1, h, zs),
                    project(x0, h, zs),
                ];
                self.quad_uv(top, &mut |u, v| {
                    let gv = (k as f32 + v.clamp(0.0, 0.999)) * roof as f32 / n as f32;
                    let sc = (gv as i32).min(roof - 1);
                    let src = g.slot_at(cx, cy - k + sc, slot);
                    g.art.comp_at(
                        src,
                        (u * 16.0).clamp(0.0, 15.0) as usize,
                        ((gv - sc as f32) * 16.0).clamp(0.0, 15.0) as usize,
                    )
                });

                // A one-cell prop stands as a flat billboard of its top layer,
                // leaning back by the camera pitch like a sprite does. This is
                // the whole reason signposts stopped being little cubes.
                if cell == Cell::Prop {
                    let (uy, uz) = (COS_P, SIN_P);
                    let q = [
                        project(x0, STEP * uy, zs + STEP * uz),
                        project(x1, STEP * uy, zs + STEP * uz),
                        project(x1, 0.0, zs),
                        project(x0, 0.0, zs),
                    ];
                    self.quad_uv(q, &mut |u, v| {
                        g.art.top_at(
                            slot,
                            (u * 16.0).clamp(0.0, 15.0) as usize,
                            (v * 16.0).clamp(0.0, 15.0) as usize,
                        )
                    });
                    continue;
                }
                if self.no_walls {
                    continue;
                }

                // South face (toward the camera).
                let hs = if cy + 1 < MapGrid::ROWS as i32 {
                    g.neighbor_h(cx, cy + 1)
                } else {
                    0.0 // the diorama's near cut plane: show the cross section
                };
                if hs < h {
                    let q = [
                        project(x0, h, zs),
                        project(x1, h, zs),
                        project(x1, hs, zs),
                        project(x0, hs, zs),
                    ];
                    if wall > 0 && k == n - 1 {
                        // The object's own drawn front, folded upright: its
                        // bottom art row lands at the bottom of the wall.
                        let base = cy - k + (n - wall);
                        self.quad_uv(q, &mut |u, v| {
                            let gv = v.clamp(0.0, 0.999) * wall as f32;
                            let sc = (gv as i32).min(wall - 1);
                            let src = g.slot_at(cx, base + sc, slot);
                            g.art.comp_at(
                                src,
                                (u * 16.0).clamp(0.0, 15.0) as usize,
                                ((gv - sc as f32) * 16.0).clamp(0.0, 15.0) as usize,
                            )
                        });
                    } else {
                        // No drawn front exists (a hedge row, a ground lip):
                        // paint it one flat material color from the cell's own
                        // art, darkened toward the floor. Folding the top
                        // texture down here is what used to make a table wear
                        // its tablecloth on its face.
                        let flat = g.art.flat[slot];
                        self.quad_uv(q, &mut |_u, v| {
                            shade(flat, 0.72 * (1.0 - 0.30 * v.clamp(0.0, 1.0)))
                        });
                    }
                }

                // West and east faces. Top-down art has no side view at all,
                // so these are always flat material, never folded.
                for (west, plane, nb) in
                    [(true, x0, g.neighbor_h(cx - 1, cy)), (false, x1, g.neighbor_h(cx + 1, cy))]
                {
                    if nb >= h {
                        continue;
                    }
                    let q = [
                        project(plane, h, zn),
                        project(plane, h, zs),
                        project(plane, nb, zs),
                        project(plane, nb, zn),
                    ];
                    let light = if west { 0.62 } else { 0.72 };
                    let flat = g.art.flat[slot];
                    self.quad_uv(q, &mut |_u, v| {
                        shade(flat, light * (1.0 - 0.30 * v.clamp(0.0, 1.0)))
                    });
                }
            }
        }
    }

    /// Sprites: group captured sprite pixels into connected figures, then draw
    /// each as ONE flat alpha-cut quad standing at its feet and leaning back
    /// by exactly the camera pitch, with a soft contact shadow. A sprite is a
    /// drawing, not an object seen from one side; no geometry is built from
    /// its pixels.
    fn render_sprites(&mut self, cap: &Capture, mgrid: &MapGrid) {
        const W: usize = ppu::WIDTH;
        let mut grid: Vec<u32> = vec![0; W * ppu::HEIGHT];
        for &(px, py, color) in &cap.sprite_pixels {
            grid[py as usize * W + px as usize] = color | 0xFF00_0000;
        }
        let mut seen = vec![false; W * ppu::HEIGHT];
        for &(sx, sy, _) in &cap.sprite_pixels {
            let start = sy as usize * W + sx as usize;
            if seen[start] {
                continue;
            }
            // Flood fill (8-connected, tolerant of 1px gaps via radius 2).
            let mut stack = vec![start];
            let mut pixels: Vec<usize> = Vec::new();
            seen[start] = true;
            while let Some(i) = stack.pop() {
                pixels.push(i);
                let (x, y) = (i % W, i / W);
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                        if nx < 0 || ny < 0 || nx >= W as i32 || ny >= ppu::HEIGHT as i32 {
                            continue;
                        }
                        let j = ny as usize * W + nx as usize;
                        if grid[j] != 0 && !seen[j] {
                            seen[j] = true;
                            stack.push(j);
                        }
                    }
                }
            }
            // Figure extents: feet = lowest pixel row.
            let feet = pixels.iter().map(|i| i / W).max().unwrap() as f32 + 1.0;
            let min_x = pixels.iter().map(|i| i % W).min().unwrap() as f32;
            let max_x = pixels.iter().map(|i| i % W).max().unwrap() as f32 + 1.0;
            // Anchor at the feet cell's GROUND elevation, never on top of a
            // blocked volume: a sprite overlapping a building on screen stands
            // on the ground behind it in world space, and the depth buffer
            // occludes it naturally (the silhouette pass shows the player
            // through).
            let (fy, fx) = (
                (feet as usize - 1).min(ppu::HEIGHT - 1),
                ((min_x + max_x) as usize / 2).min(W - 1),
            );
            let ground = match mgrid.cell_of_screen(fx, fy) {
                Cell::Water => WATER,
                _ => 0.0,
            };

            // Contact shadow first (drawn onto the ground, no z write).
            let (ccx, cw) = ((min_x + max_x) / 2.0, (max_x - min_x) / 2.0);
            let n = 10;
            for k in 0..n {
                let (a0, a1) = (
                    std::f32::consts::TAU * k as f32 / n as f32,
                    std::f32::consts::TAU * (k + 1) as f32 / n as f32,
                );
                let pt = |a: f32| {
                    let (wx, wy, wz) = world(
                        ccx + cw * 0.85 * a.cos(),
                        feet - 1.0 + cw * 0.5 * a.sin(),
                        ground + 0.15,
                    );
                    project(wx, wy, wz)
                };
                let (cxw, cyw, czw) = world(ccx, feet - 1.0, ground + 0.15);
                self.tri([project(cxw, cyw, czw), pt(a0), pt(a1)], 0, Some(0.55));
            }

            let top = pixels.iter().map(|i| i / W).min().unwrap() as f32;
            let hart = feet - top;
            // Lean-back unit vector: up tilted north by the pitch angle.
            let (uy, uz) = (COS_P, SIN_P);
            let (wx0, _, wz0) = world(min_x, feet, 0.0);
            let wx1 = wx0 + (max_x - min_x);
            let q = [
                project(wx0, ground + hart * uy, wz0 + hart * uz),
                project(wx1, ground + hart * uy, wz0 + hart * uz),
                project(wx1, ground, wz0),
                project(wx0, ground, wz0),
            ];
            let (bw, bh) = (max_x - min_x, hart);
            let mut sampler = |u: f32, v: f32| {
                let sx = (min_x + (u * bw).min(bw - 0.5)) as usize;
                let sy = (top + (v * bh).min(bh - 0.5)) as usize;
                let c = grid[sy.min(ppu::HEIGHT - 1) * W + sx.min(W - 1)];
                if c == 0 { SKIP } else { c & 0x00FF_FFFF }
            };
            self.quad_uv(q, &mut sampler);
            // Repaint the figure closest to screen center (the player) as a
            // translucent silhouette wherever scenery hides it, so walking
            // behind a house or tree never loses the character.
            let center_dist =
                (ccx - ppu::WIDTH as f32 / 2.0).abs() + (feet - ppu::HEIGHT as f32 / 2.0).abs();
            if center_dist < 40.0 {
                self.quad_uv_ghost(q, &mut sampler);
            }
        }
    }
}

impl MapGrid {
    /// Art slot of a cell in the window, falling back to `def` outside it.
    /// Only volumes taller than the north or south margin can reach outside,
    /// which happens many cells beyond the visible frame.
    #[inline]
    fn slot_at(&self, cx: i32, cy: i32, def: usize) -> usize {
        if cx < 0 || cy < 0 || cx >= Self::COLS as i32 || cy >= Self::ROWS as i32 {
            return def;
        }
        self.slots[cy as usize * Self::COLS + cx as usize] as usize
    }
}

impl Renderer {
    /// Tilt-shift post-process, the miniature-diorama look: a horizontal
    /// band through the focus line stays sharp and the frame blurs toward
    /// the top and bottom edges (two separable gaussian passes), with a
    /// slight saturation lift. Runs on the finished 3D scene BEFORE the UI
    /// composites over it. `level` 1-3, from GBA_TILT (default 2).
    fn tilt_shift(&mut self, level: u32) {
        // (tap spacing as fraction of height, sharp half-band, blur ramp,
        // saturation), matching the reference presets.
        let presets = [
            (0.0016, 0.14, 0.42, 1.10),
            (0.0028, 0.10, 0.36, 1.18),
            (0.0042, 0.07, 0.30, 1.28),
        ];
        let (spacing, band, range, sat) = presets[(level as usize - 1).min(2)];
        let spacing = (HEIGHT as f32 * spacing).clamp(0.75, 3.0);
        const WTS: [i32; 5] = [930, 797, 498, 221, 66]; // gaussian * 4096
        let strength = |y: usize| {
            let d = (y as f32 / HEIGHT as f32 - 0.5).abs() - band;
            let s = (d / range).clamp(0.0, 1.0);
            s * s * spacing
        };
        let blur = |src: &[u32], dst: &mut [u32], horizontal: bool| {
            for y in 0..HEIGHT {
                let o = strength(y);
                let row = y * WIDTH;
                // The widest tap is (4 * o) as i32, so below 0.25 every tap
                // reads the same source pixel and the blur is a no-op. Skip
                // it: identical output, and it pays for the new per-pixel
                // tests above.
                if o * 4.0 < 1.0 {
                    dst[row..row + WIDTH].copy_from_slice(&src[row..row + WIDTH]);
                    continue;
                }
                for x in 0..WIDTH {
                    let (mut r, mut g, mut b, mut wsum) = (0i32, 0i32, 0i32, 0i32);
                    for (k, &w) in WTS.iter().enumerate() {
                        for sgn in [-1i32, 1] {
                            if k == 0 && sgn == 1 {
                                continue;
                            }
                            let off = (k as f32 * o) as i32 * sgn;
                            let (sx, sy) = if horizontal {
                                ((x as i32 + off).clamp(0, WIDTH as i32 - 1), y as i32)
                            } else {
                                (x as i32, (y as i32 + off).clamp(0, HEIGHT as i32 - 1))
                            };
                            let c = src[sy as usize * WIDTH + sx as usize];
                            r += (c >> 16 & 0xFF) as i32 * w;
                            g += (c >> 8 & 0xFF) as i32 * w;
                            b += (c & 0xFF) as i32 * w;
                            wsum += w;
                        }
                    }
                    dst[row + x] =
                        ((r / wsum) as u32) << 16 | ((g / wsum) as u32) << 8 | (b / wsum) as u32;
                }
            }
        };
        let mut tmp = vec![0u32; WIDTH * HEIGHT];
        let src = std::mem::take(&mut self.buffer);
        blur(&src, &mut tmp, true);
        self.buffer = src;
        blur(&tmp, &mut self.buffer, false);
        // Saturation lift over the whole frame sells the model-photo feel.
        for c in self.buffer.iter_mut() {
            let (r, g, b) = ((*c >> 16 & 0xFF) as f32, (*c >> 8 & 0xFF) as f32, (*c & 0xFF) as f32);
            let luma = 0.299 * r + 0.587 * g + 0.114 * b;
            let mix = |ch: f32| (luma + (ch - luma) * sat).clamp(0.0, 255.0) as u32;
            *c = mix(r) << 16 | mix(g) << 8 | mix(b);
        }
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
