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
/// Camera distance from the diorama center and focal length, at perspective
/// strength 1. Their RATIO is the scale at the middle of the frame, so
/// multiplying both by the same number keeps the diorama the same size while
/// flattening the perspective toward an orthographic view.
const BASE_DIST: f32 = 340.0;
const BASE_FOCAL: f32 = 940.0;
const CX: f32 = WIDTH as f32 / 2.0;
const CY: f32 = 236.0;

/// HOW WIDE-ANGLE THE CAMERA IS (GBA_3D_PERSP, default 6).
///
/// At 1 the camera sat 340 world units from the diorama with a 940-pixel
/// focal length, which is an extremely wide lens: a cell at the near edge of
/// the frame was drawn twice the size of a cell at the far edge, so a house
/// near the screen edge sheared into a skewed slab, roofs at the east edge
/// leaned hard, and every billboard slid sideways as it crossed the frame.
/// Pulling the camera back and lengthening the lens by the same factor keeps
/// the picture the same size at the centre and flattens all of that out; the
/// HGSS / 3dSen miniature look is a tilted near-orthographic view, not a wide
/// angle one. 1 restores the original wide lens.
fn camera_scale() -> (f32, f32) {
    static K: std::sync::OnceLock<(f32, f32)> = std::sync::OnceLock::new();
    *K.get_or_init(|| {
        let k: f32 =
            std::env::var("GBA_3D_PERSP").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
        let k = k.clamp(0.5, 64.0);
        (BASE_DIST * k, BASE_FOCAL * k)
    })
}

const BACKGROUND_TOP: u32 = 0x0016203A;
const BACKGROUND_BOT: u32 = 0x00060A14;

/// Sentinel a texture sampler returns for a transparent texel: the pixel is
/// skipped entirely (no color, no depth), cutting the silhouette out of the
/// quad.
const SKIP: u32 = 0xFFFF_FFFF;

/// Blank rows an overworld character sprite leaves at the bottom of its OAM
/// cell. Measured, not guessed: for every unclipped figure in a Pallet Town
/// walk the drawn feet sit exactly this far above the cell's bottom edge, and
/// it is the one number the OAM box cannot supply for a figure whose feet are
/// off the bottom of the screen.
const VPAD: f32 = 1.0;

/// Metatiles per row of a FRLG tileset's metatile image. A multi-metatile
/// object's cell one row down is this many ids on.
const META_ROW: u16 = 8;

/// Height of one metatile step, in world units (= GBA pixels).
const STEP: f32 = 16.0;
/// Water sits below ground so shorelines get a lip.
const WATER: f32 = -3.0;
/// How tall a tree stands, as a fraction of its own artwork.
///
/// A tree is a screen-facing billboard, so its height is spent in SCREEN
/// pixels, while the ground it stands on is foreshortened: two map rows of
/// tree measure 32 pixels of picture but only 32 * sin(pitch) = 18 pixels of
/// ground. At 1:1 every tree therefore reached 14 pixels into the tree behind
/// it and ate its trunk and the shaded ring under its canopy. Along the north
/// edge of a map that is invisible, because you look at one row of trees with
/// grass in front of it; along the WEST edge the tree line runs away from the
/// camera, so every tree was standing behind another one, nothing but pointed
/// canopy tops survived, and the whole column merged into one flat green
/// texture. Three quarters is what leaves the base of each tree showing, so a
/// receding line reads as separate trees, and it still stands a tree half
/// again as tall as the ground it covers.
const TREE_TALL: f32 = 0.75;

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
    let (dist, focal) = camera_scale();
    let yv = wy * COS_P + wz * SIN_P;
    let zv = dist + wz * COS_P - wy * SIN_P;
    (CX + focal * wx / zv, CY - focal * yv / zv, zv)
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
    /// Bottom layer only (the ground the top layer is drawn over).
    bot: Vec<u32>,
    /// Fraction of the top layer that is actually drawn (0 = the metatile's
    /// whole picture lives in the ground layer, 1 = the top layer hides it).
    cover: Vec<f32>,
    /// Lowest drawn row of the top layer, 0..15, or -1 when nothing is drawn.
    /// A billboard is stood on the ground by THIS row, not by the tile edge,
    /// which is what stops fences hovering above their own shadow.
    ymax: Vec<i32>,
    /// True when the metatile's OBJECT (its top layer, or the whole tile when
    /// the art lives in the ground layer) is dominated by green: a plant.
    /// Plants stand up as billboards, built structures are extruded as solids.
    leaf: Vec<bool>,
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
            bot: Vec::new(),
            cover: Vec::new(),
            ymax: Vec::new(),
            leaf: Vec::new(),
            flat: Vec::new(),
            dark: Vec::new(),
            slot: vec![-1; 1024],
        }
    }

    #[inline]
    fn comp_at(&self, slot: usize, x: usize, y: usize) -> u32 {
        self.comp[slot * 256 + y * 16 + x]
    }

    /// The object's own pixels: the top layer where the metatile has one,
    /// otherwise the whole tile (art drawn entirely in the ground layer has
    /// nothing behind it to show through).
    #[inline]
    fn object_at(&self, slot: usize, x: usize, y: usize) -> u32 {
        if self.cover[slot] > 0.05 {
            self.top[slot * 256 + y * 16 + x]
        } else {
            self.comp[slot * 256 + y * 16 + x]
        }
    }

    #[inline]
    fn bot_at(&self, slot: usize, x: usize, y: usize) -> u32 {
        self.bot[slot * 256 + y * 16 + x]
    }

    /// Top layer alone; SKIP where the metatile draws nothing over its ground.
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
        for q in 0..4u32 {
            let e = self.rd16(def + q * 2);
            let (ox, oy) = ((q as usize & 1) * 8, (q as usize / 2) * 8);
            self.blit_tile(e, &mut comp, ox, oy, false);
        }
        let bot = comp;
        for q in 0..4u32 {
            let e = self.rd16(def + (4 + q) * 2);
            let (ox, oy) = ((q as usize & 1) * 8, (q as usize / 2) * 8);
            self.blit_tile(e, &mut comp, ox, oy, true);
            self.blit_tile(e, &mut top, ox, oy, true);
        }
        // How much of the tile the top layer actually draws, and how far down
        // it reaches: a fence, a sign or a tree is a partial overlay standing
        // on drawn ground, while a house wall covers its tile completely.
        let mut drawn = 0u32;
        let mut ymax = -1i32;
        for (i, &c) in top.iter().enumerate() {
            if c != SKIP {
                drawn += 1;
                ymax = ymax.max((i / 16) as i32);
            }
        }
        let cover = drawn as f32 / 256.0;
        art.cover.push(cover);
        art.ymax.push(ymax);
        // Green dominance of the object itself. Judging the composite would
        // call a grey fence standing in grass a plant, so when the tile has a
        // real top layer only those pixels count.
        let (mut lr, mut lg, mut lb) = (1u32, 1u32, 1u32);
        for (i, &c) in comp.iter().enumerate() {
            if cover > 0.05 && top[i] == SKIP {
                continue;
            }
            let c = if cover > 0.05 { top[i] } else { c };
            lr += c >> 16 & 0xFF;
            lg += c >> 8 & 0xFF;
            lb += c & 0xFF;
        }
        art.leaf.push(lg * 100 > lr * 118 && lg * 100 > lb * 118);
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
        art.bot.extend_from_slice(&bot);
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
    /// A one-cell prop that stands on the ground rather than being part of it:
    /// a fence post, a signpost, a railing. Rendered as the ground underneath
    /// (the metatile's BOTTOM layer, so the prop is not also painted flat on
    /// the floor) plus one upright billboard of the prop's own art.
    Bill,
    /// A cell belonging to a TREE UNIT: the repeating multi-metatile graphic
    /// FireRed tiles its tree borders out of. `w` x `h` is the unit's size in
    /// cells and (`dx`, `dy`) is this cell's place inside it; only the unit's
    /// north-west cell carries the billboard, which is the unit's WHOLE
    /// composed picture as one quad. Every cell of the unit draws shadowed
    /// grass as its floor — never its own art, which is what used to lay
    /// canopies out flat as pale shelves.
    Tree { w: u8, h: u8, dx: u8, dy: u8 },
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
    /// Map cell coordinates of window cell (0, 0), and the player's map cell.
    /// Only used by the trace/regression harness, which needs every number it
    /// checks to be expressed in MAP space so two camera positions can be
    /// compared cell for cell.
    pub gx0: i32,
    pub gy0: i32,
    pub player: (i32, i32),
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

/// What a blocked map cell actually is, judged from its artwork.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Not blocked geometry at all (walkable, water, void).
    Open,
    /// A plant: green artwork. Stands up as a billboard.
    Plant,
    /// A fence, railing, sign or post: a partial overlay in a line one cell
    /// thick. Stands up as a billboard, one per cell.
    Thin,
    /// A built volume: house, cliff, counter. Extruded.
    Struct,
}

/// Camera state carried between frames: the last raw scroll registers, the
/// integrated camera position in map pixels, and which map it belongs to.
#[derive(Clone, Copy)]
struct Cam {
    hofs: i32,
    vofs: i32,
    x: i32,
    y: i32,
    map: u32,
}

thread_local! {
    static CAM: std::cell::Cell<Option<Cam>> = const { std::cell::Cell::new(None) };
}

/// Southern rows of a built volume that stand up as its front (GBA_3D_WALL,
/// default 2). See `structure()` for why this is the knob that controls how
/// much ground a building hides.
fn wall_steps() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("GBA_3D_WALL").ok().and_then(|v| v.parse().ok()).unwrap_or(2).clamp(1, 3)
    })
}

/// GBA_3D_TRACE=1: emit one machine-readable line per frame for every quantity
/// the regression harness checks (camera, per-figure anchors, geometry hash),
/// so smoothness, anchor stability and geometry stability are all decided by
/// numbers instead of by looking at pictures.
pub fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GBA_3D_TRACE").is_ok())
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
    pub fn read(bus: &Bus, cap: Option<&Capture>) -> Option<MapGrid> {
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

        // THE REAL MAP'S OWN SIZE, and which sides of it lead somewhere.
        //
        // The 7-cell margin around the live grid is two different things at
        // once. On a side with a map connection it holds the NEIGHBOURING
        // map's real cells, which is why walking to the south edge of Pallet
        // shows Route 21 coming up. On a side with no connection it holds the
        // map's border block, tiled: for an interior that is the black padding
        // FireRed hides off the edge of a 240x160 screen, plus, at a door, the
        // bottom half of the doormat repeated forever. Drawing it was the
        // "duplicated mat fragment and black strip floating below the floor".
        //
        // MapLayout in FRLG: +0x00 width, +0x04 height, +0x08 border,
        // +0x0C map, +0x10 primary tileset, +0x14 secondary tileset.
        let (mapw, maph) = (rd32(layout)? as i32, rd32(layout + 4)? as i32);
        if !(1..=1024).contains(&mapw) || !(1..=1024).contains(&maph) {
            return None;
        }
        // MapHeader +0x0C -> {s32 count; MapConnection *list}, each entry 12
        // bytes starting with the direction (1 south, 2 north, 3 west, 4 east).
        let mut conn = 0u8;
        if let Some(p) = rd32(0x0203_6DFC + 0x0C).filter(|&p| p >> 24 == 8 || p >> 24 == 9)
            && let (Some(count), Some(list)) = (rd32(p), rd32(p + 4))
            && list >> 24 == 8
        {
            for i in 0..count.min(16) {
                if let Some(d) = rd8(list + i * 12)
                    && (1..=4).contains(&d)
                {
                    conn |= 1 << d;
                }
            }
        }
        if dbg {
            eprintln!("grid: map {mapw}x{maph} conn={conn:02X}");
        }
        // Live grid entry: metatile id 0-9, collision 10-11, elevation 12-15.
        // VMap coordinates are map coordinates + 7 (the border margin).
        let entry = |gx: i32, gy: i32| -> Option<u16> {
            // Outside the real map the cell only exists if a connection leads
            // that way; a corner is beyond two edges at once and is always
            // filler, so the diorama is cut cleanly at the map boundary.
            let out_we = if gx < 0 { 3 } else if gx >= mapw { 4 } else { 0 };
            let out_ns = if gy < 0 { 2 } else if gy >= maph { 1 } else { 0 };
            if out_we != 0 && out_ns != 0 {
                return None;
            }
            for d in [out_we, out_ns] {
                if d != 0 && conn & 1 << d == 0 {
                    return None;
                }
            }
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
        let attrs = |e: u16| -> u32 {
            let m = (e & 0x3FF) as u32;
            let a = if m < 0x280 { rd32(aprim + m * 4) } else { rd32(asec + (m - 0x280) * 4) };
            a.unwrap_or(0)
        };
        let behavior = |e: u16| attrs(e) & 0x1FF;
        let is_water = |e: u16| (0x10..=0x2F).contains(&behavior(e));
        // A cell is solid geometry when the game blocks movement into it and
        // it is not water (water is blocked too, until you have Surf).
        let solid = |gx: i32, gy: i32| -> bool {
            entry(gx, gy).is_some_and(|e| e >> 10 & 3 != 0 && !is_water(e))
        };

        // Screen top-left cell: the player cell is centered at screen cell
        // (7, 5); fine scroll from the ground layer's BG registers.
        let r16io = |off: usize| u16::from_le_bytes([bus.io[off], bus.io[off + 1]]);
        let (hofs, vofs) = ((r16io(0x18) & 0x1FF) as i32, (r16io(0x1A) & 0x1FF) as i32);
        let src = ArtSource {
            rom: &bus.rom,
            vram: &bus.vram,
            palette: &bus.palette,
            meta: (mprim, msec),
            // The map layers all share one char block; take it from the
            // ground layer's control register rather than assuming zero.
            char_base: ((r16io(0x0C) >> 2) & 3) as usize * 0x4000,
        };
        let art = std::cell::RefCell::new(Art::new());
        let slot_of = |e: u16| -> usize { src.decode(&mut art.borrow_mut(), e) };

        // The camera has to be ONE continuous quantity. The player's map
        // coordinate is not it: FireRed snaps that to the destination cell on
        // the first frame of a step and then slides the hardware scroll there
        // over the following sixteen frames. Building the camera out of
        // "player cell * 16 + scroll fraction" therefore jumped a whole cell
        // forward at each step and slid a whole cell back as the fraction
        // wrapped, which is the bounce. So we integrate the scroll registers
        // (continuous, but only known modulo 512) and use the player cell just
        // to place that reading in map space when we first pick it up or when
        // the map changes under us.
        let snap = (px * 16 + hofs.rem_euclid(16), py * 16 + vofs.rem_euclid(16));
        let prev = CAM.with(|c| c.get());
        let (mut camx, mut camy) = match prev {
            Some(p) if p.map == grid_ptr => {
                // The scroll registers wrap (FireRed keeps them inside one
                // 256-pixel BG map), so a frame's motion is the smallest
                // signed difference, never the raw one.
                let d = |now: i32, was: i32| (now - was + 128).rem_euclid(256) - 128;
                (p.x + d(hofs, p.hofs), p.y + d(vofs, p.vofs))
            }
            _ => snap,
        };
        // A warp, a door or a camera cut moves the world by more than a step
        // can account for: re-anchor rather than drift.
        if (camx - snap.0).abs() > 48 {
            camx = snap.0;
        }
        if (camy - snap.1).abs() > 48 {
            camy = snap.1;
        }
        // IS THIS THE OVERWORLD, AND WHERE IS THE CAMERA REALLY?
        //
        // Those are the same question, and the picture the PPU has just drawn
        // answers it. If the camera is right then the metatile the map grid
        // puts under a screen pixel is the colour the PPU drew there, so a
        // candidate camera can be SCORED by sampling the background composite
        // and counting agreements.
        //
        // That settles two separate bugs at once, and both were caused by
        // assuming instead of measuring.
        //
        // FireRed does not always lock the camera to the player. It stops
        // following near a map edge and during scripted movement while the
        // player keeps walking, so a camera derived from the player's own
        // coordinate is offset by exactly the amount the camera stopped
        // following -- which is why the player and the NPCs beside him were
        // drawn one or two rows off the floor, and why the offset picked up
        // inside a building was still there after stepping outside. The
        // best-scoring whole-cell offset IS the camera, remeasured every frame,
        // so a warp resyncs on the first frame with no history to carry.
        //
        // And when nothing scores at all -- a battle, a full-screen menu, a
        // summary screen -- the map grid still READS perfectly well but
        // describes a map that is not the picture on the screen. Pointer
        // validity cannot tell the difference; agreement with the drawn pixels
        // can, and the honest answer is "no overworld here", which drops the
        // frame to flat 2D exactly the way a battle always used to.
        if let Some(cap) = cap.filter(|c| c.bg_frame.len() >= ppu::WIDTH * ppu::HEIGHT) {
            let score = |cx: i32, cy: i32| -> (u32, u32) {
                let (mut hit, mut n) = (0u32, 0u32);
                for sy in (4..ppu::HEIGHT).step_by(10) {
                    for sx in (4..ppu::WIDTH).step_by(10) {
                        let (mx, my) = (cx + sx as i32 - 112, cy + sy as i32 - 80);
                        let Some(e) = entry(mx.div_euclid(16), my.div_euclid(16)) else {
                            continue;
                        };
                        let s = slot_of(e);
                        let a = art.borrow().comp_at(
                            s,
                            mx.rem_euclid(16) as usize,
                            my.rem_euclid(16) as usize,
                        );
                        n += 1;
                        hit += (a == cap.bg_frame[sy * ppu::WIDTH + sx]) as u32;
                    }
                }
                (hit, n)
            };
            // The fine scroll comes straight from the ground layer's own
            // register and is never wrong, so only whole cells are searched.
            let mut best = (score(camx, camy), 0i32, 0i32);
            if best.0 .0 * 4 < best.0 .1 * 3 {
                for r in 1..=3i32 {
                    for dy in -r..=r {
                        for dx in -r..=r {
                            if dx.abs().max(dy.abs()) != r {
                                continue;
                            }
                            let s = score(camx + dx * 16, camy + dy * 16);
                            if s.0 * best.0 .1 > best.0 .0 * s.1 {
                                best = (s, dx, dy);
                            }
                        }
                    }
                    if best.0 .0 * 4 >= best.0 .1 * 3 {
                        break;
                    }
                }
            }
            let (hit, n) = best.0;
            if std::env::var("GBA_3D_MATCH").is_ok() {
                eprintln!(
                    "match: {hit}/{n} = {:.2} offset ({},{})",
                    hit as f32 / n.max(1) as f32,
                    best.1,
                    best.2
                );
            }
            // Too little of the map falls on screen to judge (a tiny interior
            // seen from its corner): trust the integrated camera. Otherwise a
            // picture the map cannot explain is not the overworld.
            if n >= 24 {
                if hit * 100 < n * 40 {
                    return None;
                }
                camx += best.1 * 16;
                camy += best.2 * 16;
            }
        }
        CAM.with(|c| c.set(Some(Cam { hofs, vofs, x: camx, y: camy, map: grid_ptr })));
        let fine = (camx.rem_euclid(16) as usize, camy.rem_euclid(16) as usize);
        let (gx0, gy0) = (camx.div_euclid(16) - 7 - MX, camy.div_euclid(16) - 5 - MY_N);
        if std::env::var("GBA_3D_CAM").is_ok() {
            eprintln!("cam: player=({px},{py}) scroll=({hofs},{vofs}) camera=({camx},{camy})");
        }

        // FIRERED'S BLACK PADDING EDGE.
        //
        // An interior's map is a little bigger than its room: the outermost
        // row or column is black filler the game keeps just off the bottom of
        // a 240x160 screen. Most of those metatiles are wholly black and the
        // all-black test already cuts them, but the ones under a doorway carry
        // the bottom half of the doormat, so the diorama grew a duplicated mat
        // fragment and a black strip hanging under the floor's south edge.
        //
        // A single half-lit tile cannot be judged on its own -- plenty of real
        // scenery is dark. A whole EDGE of the map that is almost entirely
        // black is unmistakable, so the test is made per edge, over the map's
        // full width or height. That keeps the answer in map coordinates and
        // therefore identical from every camera position, which the geometry
        // check requires.
        let dark_line = |horizontal: bool, at: i32| -> bool {
            let n = if horizontal { mapw } else { maph };
            let mut dark = 0;
            for i in 0..n {
                let (gx, gy) = if horizontal { (i, at) } else { (at, i) };
                let d = entry(gx, gy).is_some_and(|e| {
                    let s = slot_of(e);
                    art.borrow().dark[s]
                });
                dark += d as i32;
            }
            dark * 4 >= n * 3
        };
        // A WARP FADE IS NOT A BLACK MAP. Every palette on screen goes to
        // black over about twenty frames when a door swallows the player, so
        // for those frames every metatile decodes as black and the padding and
        // doorway tests below would cut the entire world away, leaving the
        // figures hanging over nothing. When the whole map reads black it is
        // the lights going out, not padding, so nothing is cut.
        let fading = {
            let (mut dark, mut n) = (0, 0);
            let mut gy = 0;
            while gy < maph {
                let mut gx = 0;
                while gx < mapw {
                    n += 1;
                    dark += entry(gx, gy).is_none_or(|e| {
                        let s = slot_of(e);
                        art.borrow().dark[s]
                    }) as i32;
                    gx += 4;
                }
                gy += 4;
            }
            dark * 10 >= n * 9
        };
        let edge_dark = [
            dark_line(true, 0),
            dark_line(true, maph - 1),
            dark_line(false, 0),
            dark_line(false, mapw - 1),
        ];
        let padding = |gx: i32, gy: i32| -> bool {
            if fading {
                return false;
            }
            (gy == 0 && edge_dark[0])
                || (gy == maph - 1 && edge_dark[1])
                || (gx == 0 && edge_dark[2])
                || (gx == mapw - 1 && edge_dark[3])
        };
        let is_dark = |gx: i32, gy: i32| -> bool {
            entry(gx, gy).is_none_or(|e| {
                let s = slot_of(e);
                art.borrow().dark[s]
            }) || padding(gx, gy)
        };
        // A BLACK CELL IS ONLY THE VOID WHEN IT IS SURROUNDED BY BLACK.
        //
        // FireRed pads the space around an interior with black metatiles, and
        // cutting those out is what lets a room read as an island rather than
        // as a floor slab in a black box. But an OPEN DOOR is a black metatile
        // too, in the middle of a building's front. Cutting it punched a hole
        // through the wall, and the cell behind the hole then showed its own
        // south face full height: the "giant dark slab hanging from the
        // roofline" that appeared the moment a door started to open. Real
        // padding comes in fields, a doorway is one black cell in a wall, so
        // the test is on the neighbourhood, not on the cell.
        let blackout = |gx: i32, gy: i32| -> bool {
            if fading || !is_dark(gx, gy) {
                return false;
            }
            let mut n = 0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    n += ((dx != 0 || dy != 0) && is_dark(gx + dx, gy + dy)) as i32;
                }
            }
            n >= 5
        };

        // What KIND of thing a blocked cell holds decides its geometry, and
        // the answer has to come from the metatile's own artwork, not from how
        // long a vertical run of blocked cells happens to be. A tree border
        // and a house wall are both tall runs of blocked cells; extruding both
        // is what turned Pallet's tree line into a smeared hedge.
        let kind = |gx: i32, gy: i32| -> Kind {
            if blackout(gx, gy) {
                return Kind::Open;
            }
            let Some(e) = entry(gx, gy) else { return Kind::Open };
            if e >> 10 & 3 == 0 || is_water(e) {
                return Kind::Open;
            }
            let s = slot_of(e);
            let a = art.borrow();
            if a.leaf[s] {
                return Kind::Plant;
            }
            let thin_ns = !solid(gx, gy - 1) && !solid(gx, gy + 1);
            let thin_we = !solid(gx - 1, gy) && !solid(gx + 1, gy);
            if a.cover[s] > 0.05 && a.cover[s] < 0.75 && (thin_ns || thin_we) {
                return Kind::Thin;
            }
            Kind::Struct
        };

        // Pass 1: what kind of thing each window cell holds. Classifying every
        // cell up front lets the tree pass below look at the window as a whole.
        let mut kinds = Vec::with_capacity(Self::COLS * Self::ROWS);
        for cy in 0..Self::ROWS as i32 {
            for cx in 0..Self::COLS as i32 {
                kinds.push(kind(gx0 + cx, gy0 + cy));
            }
        }
        let kind_at = |cx: i32, cy: i32| -> Kind {
            if cx < 0 || cy < 0 || cx >= Self::COLS as i32 || cy >= Self::ROWS as i32 {
                Kind::Open
            } else {
                kinds[cy as usize * Self::COLS + cx as usize]
            }
        };

        // TREE UNITS.
        //
        // FireRed's tree borders are one multi-metatile tree graphic tiled over
        // and over, its neighbours overlapping. Earlier versions rebuilt trees
        // cell by cell and stacked the pieces, which is why the border came out
        // as ragged half-canopies with shelf lines across it: a 16-pixel slice
        // of a tree is not a tree. So find the repeating unit ONCE for the
        // whole window, straight from the metatile ids, and instance the whole
        // graphic per occurrence.
        // The unit a plant cell belongs to: walk west while the id keeps
        // decrementing and north while it drops by one metatile-image row,
        // then measure the unit's extent the same way. Every step is a local test
        // on map data, so the answer for a given map cell is the same from
        // every camera position.
        // The walk runs in MAP coordinates, not window coordinates: a unit
        // straddling the window's own edge must still resolve to the same
        // unit, or the identical border would be built differently from two
        // camera positions (which the regression harness checks).
        let plant_id = |gx: i32, gy: i32| -> Option<u16> {
            // Inside the window the classification is already computed; only a
            // unit straddling the window edge pays for a fresh one.
            let (cx, cy) = (gx - gx0, gy - gy0);
            let k = if (0..Self::COLS as i32).contains(&cx) && (0..Self::ROWS as i32).contains(&cy) {
                kinds[cy as usize * Self::COLS + cx as usize]
            } else {
                kind(gx, gy)
            };
            (k == Kind::Plant).then(|| entry(gx, gy).unwrap_or(0) & 0x3FF)
        };
        // Is the cell to the east the SAME tree unit continuing, one metatile
        // to the right? Inside a tree drawn as a run of consecutive ids the
        // answer is simply id+1. FireRed's map borders are not that tidy: the
        // west tree column of an outdoor map pairs the left half of one tree
        // graphic (0x014/0x01C) with the right half of a DIFFERENT one
        // (0x017/0x01F), because the border art is built to overlap. Requiring
        // id+1 split every one of those trees into two 16-pixel slivers, each
        // stood up on its own, which is why the west border read as flat
        // tiles instead of trees. Metatiles in the image are laid out in 2x2
        // blocks, so a tree's left half always sits on an even column and its
        // right half on the odd column beside it: an even id followed by an
        // odd one in the same image row is the same tree continuing, whatever
        // the gap between the two ids.
        let hstep = |west: u16, east: u16| -> bool {
            west + 1 == east || (west & 1 == 0 && east & 1 == 1 && west >> 3 == east >> 3)
        };
        let unit = |cx: i32, cy: i32| -> (i32, i32, u8, u8) {
            let (gx, gy) = (gx0 + cx, gy0 + cy);
            let (mut ax, mut ay) = (gx, gy);
            while gx - ax < 3
                && plant_id(ax - 1, ay).zip(plant_id(ax, ay)).is_some_and(|(w, c)| hstep(w, c))
            {
                ax -= 1;
            }
            while gy - ay < 3
                && plant_id(ax, ay - 1)
                    .zip(plant_id(ax, ay))
                    .is_some_and(|(n, c)| n + META_ROW == c)
            {
                ay -= 1;
            }
            let mut w = 1;
            while w < 4
                && plant_id(ax + w, ay)
                    .zip(plant_id(ax + w - 1, ay))
                    .is_some_and(|(e, c)| hstep(c, e))
            {
                w += 1;
            }
            let mut h = 1;
            while h < 4
                && plant_id(ax, ay + h)
                    .zip(plant_id(ax, ay + h - 1))
                    .is_some_and(|(s, c)| s == c + META_ROW)
            {
                h += 1;
            }
            (ax - gx0, ay - gy0, w as u8, h as u8)
        };

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
                let slot = slot_of(e) as u32;
                // Black padding metatiles are not scenery, they are the void
                // the game hides off the edge of a 240x160 screen.
                if blackout(gx, gy) {
                    cells.push(Cell::Void);
                    slots.push(slot);
                    height.push(0.0);
                    continue;
                }
                let cell = match kind_at(cx, cy) {
                    Kind::Plant => {
                        let (ax, ay, w, h) = unit(cx, cy);
                        Cell::Tree { w, h, dx: (cx - ax) as u8, dy: (cy - ay) as u8 }
                    }
                    Kind::Thin => Cell::Bill,
                    Kind::Struct => Self::structure(gx, gy, &kind, &entry),
                    Kind::Open if is_water(e) => Cell::Water,
                    Kind::Open => match behavior(e) {
                        0x02 | 0x03 => Cell::Grass,
                        _ => Cell::Flat,
                    },
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
        // Whatever the player is standing on is never a lone volume: FireRed
        // lets him stand on a warp mat or a stair tile, and extruding that one
        // cell wrapped him in a box.
        //
        // A cell inside a BIGGER volume is a different matter, and flattening
        // it was the door-opening bug. Walking into a door puts him on a cell
        // in the middle of the building's front; zeroing that cell's height
        // punched a hole through the wall, and the cell behind the hole then
        // showed its own full-height side, which is the "giant dark slab
        // hanging from the roofline". The building stays whole, and he walks
        // into the doorway and out of sight exactly as he does in the game.
        let (pcx, pcy) = (px - gx0, py - gy0);
        if (0..Self::COLS as i32).contains(&pcx) && (0..Self::ROWS as i32).contains(&pcy) {
            let i = pcy as usize * Self::COLS + pcx as usize;
            let lone = !matches!(cells[i], Cell::Void | Cell::Block { n: 2.., .. });
            if lone {
                cells[i] = Cell::Flat;
                height[i] = 0.0;
            }
        }
        let art = art.into_inner();
        if std::env::var("GBA_3D_CELLS").is_ok() {
            let mut seen: Vec<u16> = Vec::new();
            for cy in 0..Self::ROWS as i32 {
                let mut line = String::new();
                for cx in 0..Self::COLS as i32 {
                    let (gx, gy) = (gx0 + cx, gy0 + cy);
                    match entry(gx, gy) {
                        Some(e) => {
                            let id = e & 0x3FF;
                            if !seen.contains(&id) {
                                seen.push(id);
                            }
                            line += &format!("{id:03X}{} ", if solid(gx, gy) { "*" } else { " " });
                        }
                        None => line += ".... ",
                    }
                }
                eprintln!("row {cy:2} {line}");
            }
            for id in seen {
                let s = art.slot[(id & 0x3FF) as usize];
                if s >= 0 {
                    let s = s as usize;
                    eprintln!(
                        "tile {id:03X} attr={:08X} beh={:03X} cover={:.2} ymax={} dark={}",
                        attrs(id),
                        behavior(id),
                        art.cover[s],
                        art.ymax[s],
                        art.dark[s]
                    );
                }
            }
        }
        if trace() {
            eprintln!(
                "TRACE cam {camx} {camy} fine {} {} gx0 {gx0} gy0 {gy0} player {px} {py} map {mapw} {maph}",
                fine.0, fine.1
            );
        }
        Some(MapGrid {
            fine,
            gx0,
            gy0,
            player: (px, py),
            cells,
            slots,
            height,
            art,
            ox: -(MX as f32) * STEP - fine.0 as f32 - ppu::WIDTH as f32 / 2.0,
            oz: ppu::HEIGHT as f32 / 2.0 + MY_N as f32 * STEP + fine.1 as f32,
        })
    }

    /// Decide what built volume the cell at (gx, gy) belongs to, purely from
    /// map data, so the answer is the same on every frame no matter where the
    /// cell sits on screen.
    ///
    /// A vertical run of built cells in top-down art is one object seen from
    /// above: its southern rows are the front the player sees (a house wall
    /// with a door) and its northern rows are the roof. We split the run
    /// accordingly:
    ///   * a run of one cell is a lone object and stands up as a billboard;
    ///   * a run of identical metatiles is a repeating row (a cliff face), so
    ///     every cell is its own one-step block wearing its own art;
    ///   * anything else is one object, its bottom rows (up to two) the front,
    ///     the rest the roof.
    fn structure(
        gx: i32,
        gy: i32,
        kind: &impl Fn(i32, i32) -> Kind,
        entry: &impl Fn(i32, i32) -> Option<u16>,
    ) -> Cell {
        const MAX: i32 = 8;
        let mut top = gy;
        while gy - top < MAX && kind(gx, top - 1) == Kind::Struct {
            top -= 1;
        }
        let mut bot = gy;
        while bot - gy < MAX && kind(gx, bot + 1) == Kind::Struct {
            bot += 1;
        }
        let len = (bot - top + 1) as usize;
        let j = (gy - top) as usize; // index from the run's north end
        let id = |i: usize| entry(gx, top + i as i32).unwrap_or(0) & 0x3FF;
        if len == 1 {
            return Cell::Bill;
        }
        // A BUILT VOLUME HAS TO BE THICK. A house is a blob at least two cells
        // across; a pond rim, a hedge line or a ledge is a line of blocked
        // cells one cell thick, and running the same extrusion over it turned
        // Pallet's pond into tall smooth green slabs with the rim art draped
        // flat along their tops. So a run with nothing built beside it stands
        // exactly one step high and wears its own art, which reads as the lip
        // it is.
        let thick = kind(gx - 1, gy) == Kind::Struct || kind(gx + 1, gy) == Kind::Struct;
        if !thick {
            return Cell::Block { h: STEP, n: 1, k: 0, wall: 0 };
        }
        if (0..len - 1).all(|i| id(i) == id(i + 1)) {
            return Cell::Block { h: STEP, n: 1, k: 0, wall: 0 };
        }
        // How many of a volume's southern rows stand up as its front, and so
        // how tall the volume is. This is the single number that decides how
        // far a building's picture climbs UP the screen, because a lift of h
        // world units moves a thing's image about h * COS_P * FOCAL / CAM_DIST
        // screen pixels north: at two steps a house hides the two ground rows
        // in front of it, which is what makes a one-cell walkable gap beside a
        // building read as a sliver. GBA_3D_WALL=1 halves that.
        let wall = (len - 1).min(wall_steps()) as u8;
        Cell::Block { h: STEP * wall as f32, n: len as u8, k: j as u8, wall }
    }

    /// The camera's position in map pixels. FireRed locks the camera to the
    /// player: measured over a walking step, `camy` is exactly the player
    /// cell's centre line, and it advances one pixel per frame with no jitter
    /// at all. It is the only continuous, animation-proof statement of where
    /// the player is, which is why the sprite anchor is built on it.
    #[inline]
    pub fn camera(&self) -> (i32, i32) {
        ((self.gx0 + 7 + MX) * 16 + self.fine.0 as i32, (self.gy0 + 5 + MY_N) * 16 + self.fine.1 as i32)
    }

    /// World x / world z of a map-pixel coordinate. Identical to what `world()`
    /// gives for the equivalent screen pixel, but expressed in map space so a
    /// sprite anchor can be stated in the same coordinates as the terrain.
    #[inline]
    pub fn world_of_map(&self, mx: i32, my: i32) -> (f32, f32) {
        let (camx, camy) = self.camera();
        ((mx - camx - 8) as f32, (80 - (my - camy + 80)) as f32)
    }

    /// The cell a map-pixel position falls in. `Void` means the diorama draws
    /// no floor there, so a figure anchored to it is standing in mid-air.
    pub fn cell_at_map(&self, mx: i32, my: i32) -> Cell {
        self.at(mx.div_euclid(16) - self.gx0, my.div_euclid(16) - self.gy0)
    }

    /// Ground height at a map-pixel position: the cell's own elevation, except
    /// that a character is never standing on water or on top of a solid, so
    /// those read as ordinary ground.
    pub fn ground_at_map(&self, mx: i32, my: i32) -> f32 {
        let cx = mx.div_euclid(16) - self.gx0;
        let cy = my.div_euclid(16) - self.gy0;
        match self.at(cx, cy) {
            Cell::Water => WATER,
            _ => 0.0,
        }
    }

    /// Map-pixel coordinate a GBA screen pixel looks at. Screen pixel (112, 80)
    /// is the camera, and the camera is `(gx0 + 7 + MX) * 16 + fine.0` in map
    /// pixels by construction of the window.
    pub fn map_pixel(&self, px: usize, py: usize) -> (i32, i32) {
        let camx = (self.gx0 + 7 + MX) * 16 + self.fine.0 as i32;
        let camy = (self.gy0 + 5 + MY_N) * 16 + self.fine.1 as i32;
        (camx + px as i32 - 112, camy + py as i32 - 80)
    }

    /// One line per window cell, keyed by MAP coordinate, for the harness's
    /// geometry-stability check: the same map area seen from two camera
    /// positions must produce character-for-character identical lines.
    pub fn trace_geometry(&self) {
        for cy in 0..Self::ROWS as i32 {
            for cx in 0..Self::COLS as i32 {
                let c = match self.at(cx, cy) {
                    Cell::Flat => "F".to_string(),
                    Cell::Grass => "G".to_string(),
                    Cell::Water => "W".to_string(),
                    Cell::Void => "V".to_string(),
                    Cell::Bill => "B".to_string(),
                    Cell::Tree { w, h, dx, dy } => format!("T{w}{h}{dx}{dy}"),
                    Cell::Block { h, n, k, wall } => format!("K{h}:{n}:{k}:{wall}"),
                };
                println!("GEOM {} {} {c}", self.gx0 + cx, self.gy0 + cy);
            }
        }
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
    /// While true, a textured pass counts how much of itself it covered
    /// (`cov`) and how much survived the depth test (`vis`). Sprites use it to
    /// tell "swallowed by a building" from "his feet are behind a fence".
    counting: bool,
    cov: u32,
    vis: u32,
    /// Which tree unit painted each pixel (`NO_TREE` for everything else), and
    /// the exact colour it painted there.
    ///
    /// TREES ARE THE ONE THING IN THE PICTURE THAT MUST STAY THE GAME'S OWN
    /// ARTWORK. Every outdoor map is framed by them, they are the biggest
    /// blocks of flat colour on screen, and the eye reads any drift in them as
    /// the renderer being wrong rather than as a photographic effect. The
    /// tilt-shift pass was doing both things it must not do to them: the
    /// gaussian blur made neighbouring canopies bleed through one another (the
    /// "you can see trees through trees" ghosting) and the saturation lift
    /// moved every canopy pixel OFF the game's palette (the paleness). Both
    /// are measured against the 2D render by `tools/treediff.py` and by the
    /// `trees.pixelmatch` harness check, which is why the mask exists rather
    /// than a global "turn the post-process down".
    tree_id: Vec<u16>,
    tree_paint: Vec<u32>,
    /// The tree unit currently being painted, or `NO_TREE`.
    marking: u16,
    /// GBA_3D_TRACE: only then is the painted colour worth recording.
    tracing: bool,
    /// Map coordinate of every tree unit drawn this frame, indexed by unit id.
    tree_at: Vec<(i32, i32)>,
}

const NO_TREE: u16 = u16::MAX;

/// How a textured pass treats pixels the depth buffer says are hidden.
#[derive(Clone, Copy, PartialEq)]
enum Ghost {
    /// Normal opaque pass: draw what is in front, write depth.
    Off,
    /// Repaint the hidden part of the figure in full colour, over whatever
    /// hides him. What a character walking behind a building gets.
    Solid,
    /// Repaint it as a faint silhouette. What a character behind a fence or a
    /// tree gets.
    Faint,
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
            counting: false,
            cov: 0,
            vis: 0,
            tree_id: vec![NO_TREE; WIDTH * HEIGHT],
            tree_paint: vec![0; WIDTH * HEIGHT],
            marking: NO_TREE,
            tracing: trace(),
            tree_at: Vec::new(),
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
                            self.tree_id[i] = NO_TREE;
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
        self.tri_uv_mode(p, uv, sample, Ghost::Off)
    }

    /// `ghost` other than `Off`: inverted depth test, no depth write -- used
    /// to repaint the player where scenery hides him.
    fn tri_uv_mode(
        &mut self,
        p: [(f32, f32, f32); 3],
        uv: [(f32, f32); 3],
        sample: &mut impl FnMut(f32, f32) -> u32,
        ghost: Ghost,
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
                if self.counting {
                    // Coverage has to be measured before the depth test, so
                    // this variant samples first. Only sprites use it; the
                    // world pass keeps its early depth reject.
                    let u = w0 * uv[0].0 + w1 * uv[1].0 + w2 * uv[2].0;
                    let v = w0 * uv[0].1 + w1 * uv[1].1 + w2 * uv[2].1;
                    let c = sample(u, v);
                    if c != SKIP {
                        self.cov += 1;
                        if z < self.zbuf[i] {
                            self.vis += 1;
                            self.zbuf[i] = z;
                            self.buffer[i] = c;
                            self.tree_id[i] = NO_TREE;
                        }
                    }
                    continue;
                }
                if (z < self.zbuf[i]) == (ghost == Ghost::Off) {
                    let u = w0 * uv[0].0 + w1 * uv[1].0 + w2 * uv[2].0;
                    let v = w0 * uv[0].1 + w1 * uv[1].1 + w2 * uv[2].1;
                    let c = sample(u, v);
                    if c != SKIP {
                        match ghost {
                            // BEHIND A BUILDING HE IS SIMPLY IN FRONT OF IT.
                            //
                            // Hollowed-out glass reads as the character
                            // dissolving, and against a roof that is exactly
                            // what "he sinks into the house" looks like. Every
                            // handheld Pokemon game that draws a diorama solves
                            // this the blunt way: a character standing on a
                            // walkable row behind a building is drawn WHOLE,
                            // in full colour, over the roof. There is no
                            // ambiguity to read past.
                            Ghost::Solid => self.buffer[i] = c,
                            // Anything else in front of him -- a fence, a
                            // signpost, the corner of a tree -- is small, and a
                            // faint silhouette through it is right: he must not
                            // look like he is standing SOUTH of the fence.
                            Ghost::Faint => {
                                let mix = |s: u32, a: u32| {
                                    (((a >> s & 0xFF) * 3 + (self.buffer[i] >> s & 0xFF)) / 4) << s
                                };
                                self.buffer[i] = mix(16, c) | mix(8, c) | mix(0, c);
                            }
                            Ghost::Off => {
                                self.zbuf[i] = z;
                                self.buffer[i] = c;
                                self.tree_id[i] = self.marking;
                                if self.tracing && self.marking != NO_TREE {
                                    self.tree_paint[i] = c;
                                }
                            }
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

    fn quad_uv_ghost(
        &mut self,
        q: [(f32, f32, f32); 4],
        sample: &mut impl FnMut(f32, f32) -> u32,
        mode: Ghost,
    ) {
        self.tri_uv_mode([q[0], q[1], q[2]], [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], sample, mode);
        self.tri_uv_mode([q[0], q[2], q[3]], [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0)], sample, mode);
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
        self.tree_id.fill(NO_TREE);
        self.tree_at.clear();
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
        if trace() {
            self.trace_trees();
        }
    }

    /// One line per tree unit on screen: how many pixels its billboard won,
    /// and how many of them the FINISHED frame no longer shows in the colour
    /// the game's own artwork put there. The second number is the whole
    /// question -- a tree that survives every later pass untouched is, pixel
    /// for pixel, the 2D render's tree -- and `trees.pixelmatch` requires it
    /// to be zero.
    fn trace_trees(&self) {
        let mut painted = vec![0u32; self.tree_at.len()];
        let mut off = vec![0u32; self.tree_at.len()];
        for (i, &id) in self.tree_id.iter().enumerate() {
            if id == NO_TREE {
                continue;
            }
            painted[id as usize] += 1;
            off[id as usize] += (self.buffer[i] != self.tree_paint[i]) as u32;
        }
        for (id, &(gx, gy)) in self.tree_at.iter().enumerate() {
            println!(
                "TREE {gx} {gy} painted {} offpalette {}",
                painted[id], off[id]
            );
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

                let ground = [
                    project(x0, 0.0, zn),
                    project(x1, 0.0, zn),
                    project(x1, 0.0, zs),
                    project(x0, 0.0, zs),
                ];

                // A TREE is drawn as ONE quad carrying the repeating unit's
                // whole composed graphic, exactly the way a diorama would hold
                // up the original sprite. Nothing is reconstructed per cell,
                // so there are no 16-pixel slices to leave shelf lines and no
                // half canopies at the window edge.
                if let Cell::Tree { w, h, dx, dy } = cell {
                    // The floor under every cell of the unit is the grass next
                    // door in shadow. Painting the tree's own metatile flat is
                    // what put a pale green shelf under each canopy.
                    let (gs, dark) = g.floor_near(cx, cy);
                    self.quad_uv(ground, &mut |u, v| {
                        shade(
                            g.art.comp_at(
                                gs,
                                (u * 16.0).clamp(0.0, 15.0) as usize,
                                (v * 16.0).clamp(0.0, 15.0) as usize,
                            ),
                            dark,
                        )
                    });
                    if dx != 0 || dy != 0 {
                        continue; // the unit's north-west cell carries the tree
                    }
                    let (w, h) = (w as i32, h as i32);
                    let xe = g.ox + (cx + w) as f32 * STEP;
                    let zbase = g.oz - (cy + h) as f32 * STEP; // unit's south edge
                    let (aw, ah) = ((w * 16) as f32, (h * 16) as f32);
                    let hgt = ah * TREE_TALL;
                    let (uy, uz) = (COS_P, SIN_P);
                    let q = [
                        project(x0, hgt * uy, zbase + hgt * uz),
                        project(xe, hgt * uy, zbase + hgt * uz),
                        project(xe, 0.0, zbase),
                        project(x0, 0.0, zbase),
                    ];
                    self.marking = self.tree_at.len() as u16;
                    self.tree_at.push((g.gx0 + cx, g.gy0 + cy));
                    self.quad_uv(q, &mut |u, v| {
                        let px = (u * aw).clamp(0.0, aw - 0.5);
                        let py = (v * ah).clamp(0.0, ah - 0.5);
                        let src = g.slot_at(cx + px as i32 / 16, cy + py as i32 / 16, slot);
                        let (ax, ay) = (px as usize % 16, py as usize % 16);
                        let c = g.art.comp_at(src, ax, ay);
                        // The unit's transparent edges: where the metatile
                        // draws nothing of its own over its ground layer and
                        // that ground pixel is exactly the grass beside the
                        // tree, the pixel is background, not canopy. Cutting
                        // it out is what lets a free-standing tree read as a
                        // tree rather than a rectangle of turf stood on end.
                        if g.art.top_at(src, ax, ay) == SKIP && c == g.art.comp_at(gs, ax, ay) {
                            SKIP
                        } else {
                            c
                        }
                    });
                    self.marking = NO_TREE;
                    continue;
                }

                // A one-cell prop (fence post, sign) stands up over its own
                // ground: its bottom layer IS the floor it is drawn over, so
                // the prop is not also painted flat under itself.
                if cell == Cell::Bill {
                    self.quad_uv(ground, &mut |u, v| {
                        g.art.bot_at(
                            slot,
                            (u * 16.0).clamp(0.0, 15.0) as usize,
                            (v * 16.0).clamp(0.0, 15.0) as usize,
                        )
                    });
                    // Stand the art on the ground by its lowest DRAWN row, not
                    // by the tile edge: fence art sits in the upper part of its
                    // tile, and hanging that from the tile edge left the fence
                    // floating above the grass.
                    let pad = if g.art.cover[slot] > 0.05 {
                        (15 - g.art.ymax[slot].max(0)) as f32
                    } else {
                        0.0
                    };
                    let hgt = 16.0 - pad;
                    let (uy, uz) = (COS_P, SIN_P);
                    let q = [
                        project(x0, hgt * uy, zs + hgt * uz),
                        project(x1, hgt * uy, zs + hgt * uz),
                        project(x1, 0.0, zs),
                        project(x0, 0.0, zs),
                    ];
                    self.quad_uv(q, &mut |u, v| {
                        g.art.object_at(
                            slot,
                            (u * 16.0).clamp(0.0, 15.0) as usize,
                            (v.clamp(0.0, 0.999) * hgt) as usize,
                        )
                    });
                    continue;
                }

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
        // Group sprite pixels by the OAM object that drew them. Grouping by
        // "pixels that touch each other" used to fuse the player with an NPC
        // standing right beside him into ONE figure: the pair got a single
        // billboard anchored at the lower one's feet (so the other appeared
        // lifted, and drew in front of him) under one merged blob of shadow.
        let mut group = vec![usize::MAX; 256];
        let mut boxes: Vec<[i32; 4]> = Vec::new(); // minx, maxx, miny, maxy
        let mut owner: Vec<u16> = vec![u16::MAX; W * ppu::HEIGHT];
        for &(px, py, color, obj) in &cap.sprite_pixels {
            let (x, y) = (px as i32, py as i32);
            let g = match group[obj as usize] {
                usize::MAX => {
                    group[obj as usize] = boxes.len();
                    boxes.push([x, x, y, y]);
                    boxes.len() - 1
                }
                g => {
                    let b = &mut boxes[g];
                    b[0] = b[0].min(x);
                    b[1] = b[1].max(x);
                    b[2] = b[2].min(y);
                    b[3] = b[3].max(y);
                    g
                }
            };
            grid[py as usize * W + px as usize] = color | 0xFF00_0000;
            owner[py as usize * W + px as usize] = g as u16;
        }
        // One figure CAN be several OAM objects (a big sprite split in two).
        // Merge groups that overlap on screen and stand on the same row; two
        // characters one map cell apart are 16 pixels apart, so they stay
        // separate.
        let mut find = (0..boxes.len()).collect::<Vec<usize>>();
        fn root(f: &mut [usize], mut i: usize) -> usize {
            while f[i] != i {
                f[i] = f[f[i]];
                i = f[i];
            }
            i
        }
        for a in 0..boxes.len() {
            for b in 0..a {
                let (p, q) = (boxes[a], boxes[b]);
                let overlap = p[0] <= q[1] && q[0] <= p[1] && p[2] <= q[3] && q[2] <= p[3];
                if overlap && (p[3] - q[3]).abs() <= 4 {
                    let (ra, rb) = (root(&mut find, a), root(&mut find, b));
                    find[ra] = rb;
                }
            }
        }
        let mut figures: Vec<Vec<usize>> = vec![Vec::new(); boxes.len()];
        for (i, o) in owner.iter_mut().enumerate() {
            if *o != u16::MAX {
                let r = root(&mut find, *o as usize);
                *o = r as u16;
                figures[r].push(i);
            }
        }
        // The same figures measured from OAM instead of from drawn pixels:
        // whole, unclipped, and moving with the world even when half of the
        // character is past the edge of the 240x160 frame.
        let mut oam_box: Vec<Option<[i32; 4]>> = vec![None; boxes.len()];
        for (obj, &g) in group.iter().enumerate() {
            if g == usize::MAX {
                continue;
            }
            let Some(&b) = cap.sprite_boxes.get(obj) else { continue };
            if b[2] <= b[0] {
                continue;
            }
            let r = root(&mut find, g);
            oam_box[r] = Some(match oam_box[r] {
                None => b,
                Some(o) => [o[0].min(b[0]), o[1].min(b[1]), o[2].max(b[2]), o[3].max(b[3])],
            });
        }
        // WHICH FIGURE IS THE PLAYER: EXACTLY ONE OF THEM.
        //
        // The player is drawn at the middle of the screen and is anchored to
        // the camera with a deadband, because the camera IS him. Deciding that
        // by a threshold -- "any figure whose box is within 24 pixels of the
        // screen centre" -- made every character standing in a NEIGHBOURING
        // CELL the player as well, since one cell is only sixteen pixels. Such
        // a figure was then pinned to the camera instead of to the map, so it
        // travelled with the camera while the ground slid underneath and
        // snapped back a cell at a time: the NPCs that "follow me, like a
        // mirage". Only the single closest figure can be the player.
        let mut best_player: Option<(f32, usize)> = None;
        for (fig, pixels) in figures.iter().enumerate() {
            if pixels.is_empty() {
                continue;
            }
            let feet = pixels.iter().map(|i| i / W).max().unwrap() as f32 + 1.0;
            let min_x = pixels.iter().map(|i| i % W).min().unwrap() as f32;
            let max_x = pixels.iter().map(|i| i % W).max().unwrap() as f32 + 1.0;
            let ccx = (min_x + max_x) / 2.0;
            let (dx, dy) = ((ccx - 120.0).abs(), (feet - 88.0).abs());
            if dx < 24.0 && dy < 24.0 {
                let d = dx + dy;
                if best_player.is_none_or(|(bd, _)| d < bd) {
                    best_player = Some((d, fig));
                }
            }
        }
        let player_fig = best_player.map(|(_, f)| f);

        for (fig, pixels) in figures.iter().enumerate() {
            if pixels.is_empty() {
                continue;
            }
            // Figure extents: feet = lowest pixel row.
            let mut feet = pixels.iter().map(|i| i / W).max().unwrap() as f32 + 1.0;
            let mut min_x = pixels.iter().map(|i| i % W).min().unwrap() as f32;
            let mut max_x = pixels.iter().map(|i| i % W).max().unwrap() as f32 + 1.0;
            let mut top = pixels.iter().map(|i| i / W).min().unwrap() as f32;
            // A FIGURE HALF OFF THE SCREEN IS STILL STANDING SOMEWHERE.
            //
            // Every extent above is the extent of what the PPU DREW, so an
            // edge that runs off the frame is pinned to the frame: as the
            // world scrolls, the drawn box of a character walking off the
            // bottom of the screen keeps its bottom row and loses its top,
            // and the position measured from it therefore travels with the
            // camera instead of staying on the map. That is the "NPC drifts
            // along with me and then snaps" at the edges of the picture.
            //
            // The OAM box has no such problem: it is where the game put the
            // object, clipped or not. Only the clipped edges are taken from
            // it, and each is corrected by the padding this sprite format
            // leaves between its cell and its drawing, measured off whichever
            // edges of this very figure are NOT clipped.
            if let Some(b) = oam_box[fig] {
                let padl = if min_x > 0.0 { min_x - b[0] as f32 } else { 0.0 };
                let padr = if max_x < W as f32 { b[2] as f32 - max_x } else { 0.0 };
                if min_x <= 0.0 {
                    min_x = b[0] as f32 + padr;
                }
                if max_x >= W as f32 {
                    max_x = b[2] as f32 - padl;
                }
                if feet >= ppu::HEIGHT as f32 {
                    feet = b[3] as f32 - VPAD;
                }
                if top <= 0.0 {
                    top = b[1] as f32;
                }
            }
            let (ccx, cw) = ((min_x + max_x) / 2.0, (max_x - min_x) / 2.0);
            // WHERE THE FIGURE STANDS.
            //
            // Not from his pixels. A character's drawn feet move a pixel or
            // two every animation frame, FireRed's bump animation walks the
            // sprite bodily into the obstacle and back while the character
            // never leaves his cell, and the sprite arrives at a step's
            // destination eight frames before the camera finishes scrolling
            // there. Reading a map cell off those pixels made the anchor jump
            // rows during a bump (the "walks through the fence then snaps
            // back"), and clamping the billboard into the cell it named made
            // the sprite crawl against the smoothly moving camera (the
            // jitter).
            //
            // The camera IS the player, exactly and continuously, so the
            // player's foot line is the camera's own map row. The pixels are
            // used only to say WHICH cell offset from the camera a figure is,
            // rounded to whole cells, which is immune to a few pixels of
            // animation; for the player that offset is zero on every frame of
            // a walk, a bump and a turn alike.
            let fx = ((min_x + max_x) as usize / 2).min(W - 1);
            let (camx, camy) = mgrid.camera();
            let measured = mgrid.map_pixel(fx, feet as usize);
            let player = player_fig == Some(fig);
            let (ax, ay) = if player {
                // Whole cells only, and with a deadband: a character's drawn
                // box sits about half a cell off the camera line by
                // construction, so plain rounding lands exactly on the tie and
                // flickers between two cells frame to frame. Anything under
                // three quarters of a cell is the camera's own figure.
                let snap = |d: i32| {
                    if d.abs() < 12 { 0 } else { (d as f32 / 16.0).round() as i32 * 16 }
                };
                (
                    camx + snap(measured.0 - camx),
                    camy + 8 + snap(measured.1 - camy - 8),
                )
            } else {
                // Other characters move continuously in map space, so their
                // measurement is used as is: a character's drawn foot line
                // falls on his own cell's southern edge, which is exactly
                // where a billboard should stand.
                measured
            };
            // THE GAME'S OWN COORDINATE IS THE LAST WORD ON WHERE HE IS.
            //
            // Everything above is measured off the picture, which is right
            // almost always and useless for the twenty frames of a warp fade,
            // where the sprite of the map being LEFT is still on screen over
            // the map being entered. So when the measurement disagrees with
            // gSaveBlock1Ptr by more than the one cell a walking step is worth,
            // the measurement is thrown away: the player is drawn on the cell
            // the game says he is standing on, never in the void beside it.
            let (ax, ay) = if player
                && ((ax.div_euclid(16) - mgrid.player.0).abs() > 1
                    || ((ay - 8).div_euclid(16) - mgrid.player.1).abs() > 1)
            {
                (mgrid.player.0 * 16 + 8, mgrid.player.1 * 16 + 16)
            } else {
                (ax, ay)
            };
            // A figure with no floor under it is not standing anywhere. During
            // a warp the outgoing map's characters are still being drawn over
            // the incoming map, and planting them on its void is exactly the
            // "NPC floating off the edge" report.
            if !player && mgrid.cell_at_map(ax, ay - 8) == Cell::Void {
                continue;
            }
            let ground = mgrid.ground_at_map(ax, ay - 8);
            let (_, wz0) = mgrid.world_of_map(ax, ay);
            let wx0 = world(min_x, feet, 0.0).0;
            if trace() {
                println!(
                    "FIG {fig} box {min_x} {max_x} {feet} mappix {mpx} {mpy} anchor {ax} {ay} cell {ccx2} {ccy2} ground {ground} wz {wz0} gamecell {gcx} {gcy} player {isp} void {void}",
                    mpx = measured.0,
                    mpy = measured.1,
                    ccx2 = ax.div_euclid(16),
                    ccy2 = (ay - 8).div_euclid(16),
                    gcx = mgrid.player.0,
                    gcy = mgrid.player.1,
                    isp = player as u8,
                    void = (mgrid.cell_at_map(ax, ay - 8) == Cell::Void) as u8,
                );
            }

            // Contact shadow first (drawn onto the ground, no z write).
            let cxw = ccx - ppu::WIDTH as f32 / 2.0;
            let n = 10;
            for k in 0..n {
                let (a0, a1) = (
                    std::f32::consts::TAU * k as f32 / n as f32,
                    std::f32::consts::TAU * (k + 1) as f32 / n as f32,
                );
                // The shadow hugs the cell he stands in: an ellipse as deep as
                // half his width used to spill onto the row in front of him,
                // which on a bank is the water.
                let zc = wz0 + cw * 0.35;
                let pt = |a: f32| {
                    project(cxw + cw * 0.85 * a.cos(), ground + 0.15, zc + cw * 0.35 * a.sin())
                };
                self.tri([project(cxw, ground + 0.15, zc), pt(a0), pt(a1)], 0, Some(0.55));
            }

            let hart = feet - top;
            // Lean-back unit vector: up tilted north by the pitch angle.
            let (uy, uz) = (COS_P, SIN_P);
            let wx1 = wx0 + (max_x - min_x);
            // A sprite is drawn ON the ground it stands on, so its base and
            // that ground are at exactly the same depth and fight over the
            // pixels at his feet; a hair of bias toward the camera settles it.
            //
            // The bias used to be six units, and a leaning billboard's whole
            // quad sits at ONE depth, so six units ate most of the thirteen a
            // full map row is worth. Anything standing on the row in front of
            // him -- a fence, a sign -- then lost the sort and he drew over it,
            // reading as though he were south of the fence. Two units cannot
            // beat a row.
            const BIAS: f32 = 2.0;
            let lean = |x: f32, h: f32| {
                let (a, b, c) = project(x, ground + h * uy, wz0 + h * uz);
                (a, b, c - BIAS)
            };
            let q = [
                lean(wx0, hart),
                lean(wx1, hart),
                lean(wx1, 0.0),
                lean(wx0, 0.0),
            ];
            let (bw, bh) = (max_x - min_x, hart);
            let mut sampler = |u: f32, v: f32| {
                let sx = (min_x + (u * bw).min(bw - 0.5)) as usize;
                let sy = (top + (v * bh).min(bh - 0.5)) as usize;
                let i = sy.min(ppu::HEIGHT - 1) * W + sx.min(W - 1);
                // Only this figure's own pixels: the quads of two characters
                // standing side by side overlap, and without the mask each
                // would paint bits of the other at its own depth.
                if owner[i] != fig as u16 { SKIP } else { grid[i] & 0x00FF_FFFF }
            };
            self.counting = true;
            self.cov = 0;
            self.vis = 0;
            self.quad_uv(q, &mut sampler);
            self.counting = false;
            // Repaint the figure closest to screen center (the player) as a
            // translucent silhouette wherever scenery hides it, so walking
            // behind a house or tree never loses the character. Only when
            // scenery really swallows him, though: painting him through a
            // fence that crosses his shins put him visibly in front of it,
            // which is exactly the "he looks like he is south of the fence"
            // report.
            //
            // A LIFTED ROOF IS NOT A FENCE, THOUGH, AND HALF-HIDDEN IS THE
            // WORST CASE. Standing a building two steps off the ground moves
            // its whole picture about forty screen pixels north, so it covers
            // the walkable rows BEHIND it -- and a figure walking along those
            // rows was cut off from the feet up, a little more with every step,
            // which reads as walking down a staircase. It is not a depth bug:
            // the roof really is nearer the camera than a figure one row north
            // of it, because lifting a quad by h also brings it h * sin(pitch)
            // closer. The diorama's answer to "hidden by scenery" is the
            // silhouette, so it just has to trigger, and half of him has to
            // vanish first only when the thing in front of him is waist high.
            // When the map says a built volume stands on the rows between him
            // and the camera, any occlusion at all is that volume, and the
            // silhouette comes up immediately.
            // IS A BUILDING STANDING BETWEEN HIM AND THE CAMERA?
            //
            // The probe used to read the single column of cells under his
            // anchor, and that column is a whole cell behind the picture for
            // the sixteen frames of a step: FireRed moves the sprite across
            // the gap first and the anchor follows the camera. So walking east
            // along the row behind his house, the roof started swallowing him
            // from the feet up while the map still said "nothing in front of
            // you", and no silhouette came -- the sinking, exactly as
            // reported, and it happened on the FIRST step onto every building.
            //
            // He is as wide as his cell, so the probe is as wide as he is:
            // the cell under each of his shoulders as well as under his feet.
            //
            // AND IT IS ASKED FROM BOTH OF THE TWO PLACES HE IS AT ONCE.
            //
            // The anchor rides the camera, which is continuous and matches his
            // pixels, but only reaches the cell he is walking into when the
            // step FINISHES; gSaveBlock1Ptr names that cell on the step's first
            // frame and holds it. So during the sixteen frames of a step the
            // two disagree, and asking either one alone is wrong for part of
            // every step: the camera cell is a step behind when he walks into a
            // building's shadow (the roof is already clipping his feet while
            // the probe still says the way is clear -- measured at up to eighty
            // clipped pixels for four frames before the cutout came up, the
            // "he is on the roof for a frame or two" report), and the game cell
            // is a step ahead when he walks out of it (the roof still covers
            // him while the probe already says he is clear).
            //
            // Either cell naming a volume is enough. The cutout then comes up
            // on the first frame anything can occlude him and stays up until
            // both agree he is past it, and since it only ever repaints pixels
            // that something really is covering, holding it a frame longer than
            // needed changes nothing on screen.
            let probe = |cx: i32, cy: i32| {
                (1..=3).any(|d| {
                    [-12, 0, 12].iter().any(|&sx| {
                        matches!(
                            mgrid.cell_at_map(cx + sx, cy + d * 16),
                            Cell::Block { h, .. } if h > 0.0
                        )
                    })
                })
            };
            let behind_volume = player
                && (probe(ax, ay - 8)
                    || probe(mgrid.player.0 * 16 + 8, mgrid.player.1 * 16 + 8));
            let center_dist =
                (ccx - ppu::WIDTH as f32 / 2.0).abs() + (feet - ppu::HEIGHT as f32 / 2.0).abs();
            let hidden = self.cov - self.vis;
            let ghost = center_dist < 40.0
                && if behind_volume { hidden > 0 } else { hidden * 2 > self.cov };
            if ghost {
                // Behind a building he is drawn whole and in full colour over
                // the roof; behind anything smaller a faint silhouette is
                // enough and keeps him from reading as standing in front of a
                // fence he is really behind.
                self.quad_uv_ghost(
                    q,
                    &mut sampler,
                    if behind_volume { Ghost::Solid } else { Ghost::Faint },
                );
            }
            if trace() {
                println!(
                    "HIDE {fig} player {p} behind {b} cov {c} vis {v} ghost {g}",
                    p = player as u8,
                    b = behind_volume as u8,
                    c = self.cov,
                    v = self.vis,
                    g = ghost as u8,
                );
            }
        }
    }
}

impl MapGrid {
    /// Floor art to paint under a plant, plus how much to darken it: the
    /// nearest cell that is real walkable ground, searched outward, with only
    /// a hint of shade. The 2D art draws no shadow under a tree at all, and a
    /// deep one laid a dark horizontal stripe across the bottom of every
    /// canopy row -- the band that made the border read as shelves.
    fn floor_near(&self, cx: i32, cy: i32) -> (usize, f32) {
        for r in 1..=3i32 {
            for (dx, dy) in [(0, r), (r, 0), (-r, 0), (0, -r)] {
                let (nx, ny) = (cx + dx, cy + dy);
                if matches!(self.at(nx, ny), Cell::Flat | Cell::Grass) {
                    return (self.slot_at(nx, ny, 0), 0.88);
                }
            }
        }
        (self.slot_at(cx, cy, 0), 0.70)
    }

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
        // saturation). The sharp band is wide and the ramp long, because the
        // tree borders that frame every outdoor map live at the very top and
        // bottom of the picture: a narrow band blurred them into pale mounds
        // and the saturation lift pushed them further off the 2D palette, and
        // "the trees look like cabbages" is that, not the geometry.
        let presets = [
            (0.0016, 0.22, 0.46, 1.06),
            (0.0026, 0.18, 0.40, 1.12),
            (0.0042, 0.10, 0.32, 1.22),
        ];
        let (spacing, band, range, sat) = presets[(level as usize - 1).min(2)];
        let spacing = (HEIGHT as f32 * spacing).clamp(0.75, 3.0);
        const WTS: [i32; 5] = [930, 797, 498, 221, 66]; // gaussian * 4096
        let strength = |y: usize| {
            let d = (y as f32 / HEIGHT as f32 - 0.5).abs() - band;
            let s = (d / range).clamp(0.0, 1.0);
            s * s * spacing
        };
        let keep = std::mem::take(&mut self.tree_id);
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
                    // A tree's own pixels are the game's artwork and are
                    // carried through untouched; blurring them is what let
                    // neighbouring canopies show through one another.
                    if keep[row + x] != NO_TREE {
                        dst[row + x] = src[row + x];
                        continue;
                    }
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
        // Saturation lift sells the model-photo feel, but it moves a colour
        // OFF the game's palette, so the trees are left out of it: they are
        // the biggest flat areas on screen and the only ones the eye compares
        // against its memory of the real game.
        for (c, &id) in self.buffer.iter_mut().zip(keep.iter()) {
            if id != NO_TREE {
                continue;
            }
            let (r, g, b) = ((*c >> 16 & 0xFF) as f32, (*c >> 8 & 0xFF) as f32, (*c & 0xFF) as f32);
            let luma = 0.299 * r + 0.587 * g + 0.114 * b;
            let mix = |ch: f32| (luma + (ch - luma) * sat).clamp(0.0, 255.0) as u32;
            *c = mix(r) << 16 | mix(g) << 8 | mix(b);
        }
        self.tree_id = keep;
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
