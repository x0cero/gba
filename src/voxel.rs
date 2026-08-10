//! Experimental "voxel diorama" renderer (--3d flag, native frontend only).
//!
//! Rebuilds the captured PPU layers as a tilted-perspective diorama in the
//! style of the Gen1Recomp voxel mod: the ground is a real 3D heightfield
//! (scenery layers extrude upward as blocks with shaded side walls), and
//! sprites stand up as thin vertical voxel figures anchored by a soft
//! contact shadow. Everything is software-rasterized (z-buffered), no
//! dependencies. Terrain tops are drawn as textured trapezoid row-strips
//! (exact for this camera: depth is constant along a screen scanline of a
//! flat strip), which keeps a busy forest scene around ~2ms.
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

/// Extrusion heights per scenery class, in world units.
const H_OVERLAY: f32 = 16.0; // BG1: house bodies, roofs, tree canopies
const H_PROP: f32 = 5.5; // BG3: fences, signs, mailboxes
const H_GRASS: f32 = 1.5; // BG3 tiles that are dominantly green: tall grass
/// Terrain height is clamped to 2 units per map pixel of distance from the
/// frame edge, so scenery scrolling in grows out of the ground instead of
/// popping in as a bare wall slab.
const EDGE_RAMP: f32 = 2.0;
const BACKGROUND_TOP: u32 = 0x0016203A;
const BACKGROUND_BOT: u32 = 0x00060A14;

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

/// Map a capture pixel (px, py) plus height to world space: the map lies in
/// the ground plane, centered, with the top of the GBA screen farthest away.
#[inline]
fn world(px: f32, py: f32, h: f32) -> (f32, f32, f32) {
    (px - ppu::WIDTH as f32 / 2.0, h, ppu::HEIGHT as f32 / 2.0 - py)
}

/// One screen-visible map cell classified from the game's own state.
#[derive(Clone, Copy, PartialEq)]
pub enum Cell {
    Flat,
    Grass, // MB_TALL_GRASS etc: a walk-through overlay, never geometry
    Water, // flat, slightly sunken so shorelines get a lip
    Block(f32, u8, u8), // blocked volume: (height, rows above, column rows)
}

/// The visible portion of FireRed's live map grid, read straight from
/// emulator RAM each frame (the reference mod's approach: geometry comes
/// from the game's collision/behavior data, not from art color guesses).
pub struct MapGrid {
    /// Fine scroll of the top-left visible cell, 0..15 pixels.
    pub fine: (usize, usize),
    /// COLS x ROWS cells covering the screen (plus one for fine scroll).
    pub cells: Vec<Cell>,
}

impl MapGrid {
    pub const COLS: usize = ppu::WIDTH / 16 + 1;
    pub const ROWS: usize = ppu::HEIGHT / 16 + 1;

    /// FireRed (US Rev 1) addresses: gBackupMapLayout (width, height, grid
    /// pointer; the grid is the LIVE map with a 7-cell border margin),
    /// gMapHeader (-> ROM map layout -> tilesets -> metatile attributes),
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
        let sb1 = rd32(0x0300_5008)?;
        if sb1 >> 24 != 2 {
            return None;
        }
        let px = rd16(sb1)? as i16 as i32;
        let py = rd16(sb1 + 2)? as i16 as i32;
        let layout = rd32(0x0203_6DFC)?;
        let attrs = |tileset: u32| -> Option<u32> {
            let t = rd32(tileset)?;
            (t >> 24 == 8 || t >> 24 == 9).then_some(t).and_then(|t| rd32(t + 0x10))
        };
        let (prim, sec) = (attrs(layout + 0x10)?, attrs(layout + 0x14)?);

        // Live grid entry: metatile id 0-9, collision 10-11, elevation 12-15.
        // VMap coordinates are map coordinates + 7 (the border margin).
        let entry = |gx: i32, gy: i32| -> Option<u16> {
            let (vx, vy) = (gx + 7, gy + 7);
            if vx < 0 || vy < 0 || vx >= vwidth || vy >= vheight {
                return None;
            }
            rd16(grid_ptr + ((vy * vwidth + vx) * 2) as u32)
        };
        let behavior = |e: u16| -> u32 {
            let m = (e & 0x3FF) as u32;
            let a = if m < 0x280 {
                rd32(prim + m * 4)
            } else {
                rd32(sec + (m - 0x280) * 4)
            };
            a.unwrap_or(0) & 0x3FF
        };
        let blocked = |gx: i32, gy: i32| entry(gx, gy).is_some_and(|e| e >> 10 & 3 != 0);

        // Screen top-left cell: the player cell is centered at screen cell
        // (7, 5); fine scroll from the ground layer's BG registers.
        let r16io = |off: usize| u16::from_le_bytes([bus.io[off], bus.io[off + 1]]);
        let (hofs, vofs) = (r16io(0x18) & 0x1FF, r16io(0x1A) & 0x1FF);
        let (gx0, gy0) = (px - 7, py - 5);

        let mut cells = Vec::with_capacity(Self::COLS * Self::ROWS);
        for cy in 0..Self::ROWS as i32 {
            for cx in 0..Self::COLS as i32 {
                let (gx, gy) = (gx0 + cx, gy0 + cy);
                let Some(e) = entry(gx, gy) else {
                    cells.push(Cell::Flat);
                    continue;
                };
                if e >> 10 & 3 != 0 {
                    // Volume height from the map's own tiling, the repeat
                    // detector idea: walk up the blocked column collecting
                    // metatile ids; a repeating pattern (period 1 or 2) is
                    // rows of one thing -- trees -- so the volume is one
                    // period tall, while a distinct stack (a house) rises
                    // with it, capped at 3 rows (48 px).
                    let mut ids = vec![e & 0x3FF];
                    for k in 1..=5 {
                        if !blocked(gx, gy - k) {
                            break;
                        }
                        ids.push(entry(gx, gy - k).unwrap_or(0) & 0x3FF);
                    }
                    let period = |p: usize| {
                        ids.len() > p && (0..ids.len() - p).all(|i| ids[i] == ids[i + p])
                    };
                    // (height, rows-above-in-column, total column rows):
                    // the renderer folds the bottom `height` pixels of the
                    // column's drawing onto the south wall and stretches
                    // the remaining top rows over the top face as the roof.
                    let above = ids.len() - 1;
                    let mut below = 0usize;
                    while below < 5 && blocked(gx, gy + below as i32 + 1) {
                        below += 1;
                    }
                    let (h, t, r) = if period(1) {
                        (16.0, 0u8, 1u8)
                    } else if period(2) {
                        (32.0, (above % 2) as u8, 2)
                    } else if above + below == 0 {
                        (10.0, 0, 1) // lone prop: mailbox, sign, fence piece
                    } else {
                        let total = (above + below + 1).min(6);
                        ((total.min(2) * 16) as f32, above.min(5) as u8, total as u8)
                    };
                    cells.push(Cell::Block(h, t, r));
                } else {
                    let b = behavior(e);
                    cells.push(match b {
                        0x02 | 0x03 => Cell::Grass,
                        0x10..=0x2F => Cell::Water,
                        _ => Cell::Flat,
                    });
                }
            }
        }
        Some(MapGrid {
            fine: ((hofs % 16) as usize, (vofs % 16) as usize),
            cells,
        })
    }
}

pub struct Renderer {
    pub buffer: Vec<u32>,
    zbuf: Vec<f32>,
    /// Per-map-pixel terrain height, from the metatile classifier.
    height: Vec<f32>,
    background: Vec<u32>,
    /// Per-map-pixel color for terrain TOP faces: normally the drawn frame,
    /// but block cells wear their column crown's art (roof rows) on top.
    top_tex: Vec<u32>,
    /// Tilt-shift level 0-3 from GBA_TILT (default 2; 0 = off).
    tilt: u32,
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
            height: vec![0.0; ppu::WIDTH * ppu::HEIGHT],
            top_tex: vec![0; ppu::WIDTH * ppu::HEIGHT],
            background,
            tilt: std::env::var("GBA_TILT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2)
                .min(3),
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
                                (((a >> s & 0xFF) * 2 + (self.buffer[i] >> s & 0xFF)) / 3) << s
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
    fn quad_uv(
        &mut self,
        q: [(f32, f32, f32); 4],
        sample: &mut impl FnMut(f32, f32) -> u32,
    ) {
        self.tri_uv([q[0], q[1], q[2]], [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], sample);
        self.tri_uv([q[0], q[2], q[3]], [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0)], sample);
    }

    fn quad_uv_ghost(
        &mut self,
        q: [(f32, f32, f32); 4],
        sample: &mut impl FnMut(f32, f32) -> u32,
    ) {
        self.tri_uv_mode([q[0], q[1], q[2]], [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], sample, true);
        self.tri_uv_mode([q[0], q[2], q[3]], [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0)], sample, true);
    }

    /// Flat textured row-strip: map pixels px0..px1 of row py at height h,
    /// colored from `row`. Depth is constant along each screen scanline of
    /// the strip, so linear interpolation is exact for this camera.
    fn strip(&mut self, px0: usize, px1: usize, py: usize, h: f32, row: &[u32]) {
        let (xtl, yt, zt) = {
            let (wx, wy, wz) = world(px0 as f32, py as f32, h);
            project(wx, wy, wz)
        };
        let (xtr, _, _) = {
            let (wx, wy, wz) = world(px1 as f32, py as f32, h);
            project(wx, wy, wz)
        };
        let (xbl, yb, zb) = {
            let (wx, wy, wz) = world(px0 as f32, py as f32 + 1.0, h);
            project(wx, wy, wz)
        };
        let (xbr, _, _) = {
            let (wx, wy, wz) = world(px1 as f32, py as f32 + 1.0, h);
            project(wx, wy, wz)
        };
        let (sy0, sy1) = ((yt - 0.5).ceil() as i32, (yb - 0.5).ceil() as i32);
        let n = px1 - px0;
        for sy in sy0.max(0)..sy1.min(HEIGHT as i32) {
            let t = (sy as f32 + 0.5 - yt) / (yb - yt);
            let xl = xtl + (xbl - xtl) * t;
            let xr = xtr + (xbr - xtr) * t;
            let z = zt + (zb - zt) * t;
            let (sx0, sx1) = ((xl - 0.5).ceil() as i32, (xr - 0.5).ceil() as i32);
            let du = n as f32 / (xr - xl);
            let base = sy as usize * WIDTH;
            for sx in sx0.max(0)..sx1.min(WIDTH as i32) {
                let i = base + sx as usize;
                if z < self.zbuf[i] {
                    let u = ((sx as f32 + 0.5 - xl) * du) as usize;
                    self.zbuf[i] = z;
                    self.buffer[i] = row[px0 + u.min(n - 1)];
                }
            }
        }
    }

    /// Terrain heights from the game's own map grid (preferred path):
    /// walkable and tall-grass cells are flat ground, water recesses for a
    /// shoreline lip, collision volumes take their measured height. The
    /// screen-edge ramp keeps scenery growing out of the ground.
    fn build_heights_from_grid(&mut self, grid: &MapGrid, cap: &Capture) {
        for py in 0..ppu::HEIGHT {
            let cy = ((py + grid.fine.1) / 16).min(MapGrid::ROWS - 1);
            // Ramp only the top and side edges: the bottom (near) edge reads
                // as the diorama's sheer cross-section cut.
            let edge_y = py as f32;
            for px in 0..ppu::WIDTH {
                let cx = ((px + grid.fine.0) / 16).min(MapGrid::COLS - 1);
                let i = py * ppu::WIDTH + px;
                let (h, t, r) = match grid.cells[cy * MapGrid::COLS + cx] {
                    Cell::Flat | Cell::Grass => (0.0, 0, 1),
                    Cell::Water => (-3.0, 0, 1),
                    Cell::Block(h, t, r) => (h, t as usize, r.max(1) as usize),
                };
                let edge = edge_y.min(px.min(ppu::WIDTH - 1 - px) as f32);
                self.height[i] = h.min((edge + 1.0) * EDGE_RAMP);
                // Fold: the bottom `h` pixels of the column's drawing are on
                // the south wall; the top face wears what remains (the roof
                // rows), stretched over the whole footprint. Fully folded
                // columns (trees) wear their top row.
                let src = if h <= 0.0 {
                    py
                } else {
                    // Screen y of the column's top edge.
                    let in_cell = (py + grid.fine.1) % 16;
                    let coltop = py as i32 - (t * 16 + in_cell) as i32;
                    let roof = ((r * 16) as f32 - h).max(16.0);
                    let d = (py as i32 - coltop) as f32 * roof / (r * 16) as f32;
                    (coltop + d as i32).clamp(0, ppu::HEIGHT as i32 - 1) as usize
                };
                self.top_tex[i] = cap.bg_frame[src * ppu::WIDTH + px];
            }
        }
    }

    /// Terrain height per map pixel. Classified per 16x16 metatile aligned
    /// to the BG scroll (FireRed's map blocks are 16x16): BG1 overlay tiles
    /// (houses, roofs, tree canopies) extrude tall, BG3 prop tiles split by
    /// color into low tall-grass (dominantly green) and fences/signs; brown
    /// prop tiles directly south of a tall tile merge into it (tree trunks
    /// under canopies), so trees read as single solid blocks. Heights ramp
    /// down near the frame edges so scenery scrolling in grows out of the
    /// ground instead of popping in as a bare side wall.
    fn build_heights(&mut self, cap: &Capture) {
        self.top_tex.copy_from_slice(&cap.bg_frame);
        let (hofs, vofs) = cap.scroll;
        let (ox, oy) = (-((hofs % 16) as i32), -((vofs % 16) as i32));
        let (tw, th) = (ppu::WIDTH.div_ceil(16) + 1, ppu::HEIGHT.div_ceil(16) + 1);
        let mut tiles = vec![0.0f32; tw * th];
        let mut green = vec![false; tw * th];
        for ty in 0..th {
            for tx in 0..tw {
                let (x0, y0) = (ox + tx as i32 * 16, oy + ty as i32 * 16);
                let (mut n_over, mut n_prop) = (0u32, 0u32);
                let (mut rs, mut gs) = (0u32, 0u32);
                for y in y0.max(0)..(y0 + 16).min(ppu::HEIGHT as i32) {
                    for x in x0.max(0)..(x0 + 16).min(ppu::WIDTH as i32) {
                        let i = y as usize * ppu::WIDTH + x as usize;
                        match cap.bg_layer[i] {
                            1 => n_over += 1,
                            3 => {
                                n_prop += 1;
                                rs += cap.bg_frame[i] >> 16 & 0xFF;
                                gs += cap.bg_frame[i] >> 8 & 0xFF;
                            }
                            _ => {}
                        }
                    }
                }
                let ti = ty * tw + tx;
                if n_over >= 40 && n_over >= n_prop {
                    tiles[ti] = H_OVERLAY;
                } else if n_prop >= 40 {
                    green[ti] = gs > rs + rs / 4;
                    tiles[ti] = if green[ti] { H_GRASS } else { H_PROP };
                }
            }
        }
        // Merge pass: fence/sign-height tiles under a tall tile are tree
        // trunks; raise them so each tree is one solid block.
        for ty in 1..th {
            for tx in 0..tw {
                let ti = ty * tw + tx;
                if tiles[ti] == H_PROP && !green[ti] && tiles[ti - tw] == H_OVERLAY {
                    tiles[ti] = H_OVERLAY;
                }
            }
        }
        for py in 0..ppu::HEIGHT {
            let ty = ((py as i32 - oy) / 16) as usize;
            // Ramp only the top and side edges: the bottom (near) edge reads
                // as the diorama's sheer cross-section cut.
            let edge_y = py as f32;
            for px in 0..ppu::WIDTH {
                let tx = ((px as i32 - ox) / 16) as usize;
                let edge = edge_y.min(px.min(ppu::WIDTH - 1 - px) as f32);
                self.height[py * ppu::WIDTH + px] =
                    tiles[ty * tw + tx].min((edge + 1.0) * EDGE_RAMP);
            }
        }
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

    /// Draw one diorama frame from the captured layers. `flat` is the PPU's
    /// real 2D framebuffer, used verbatim when the scene is not a mode-0
    /// overworld (intro, battles) or when UI covers most of the picture.
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
        match grid {
            Some(g) => self.build_heights_from_grid(g, cap),
            None => self.build_heights(cap),
        }

        // Terrain: textured strips over constant-height runs, then walls at
        // height discontinuities.
        for py in 0..ppu::HEIGHT {

            let hrow = py * ppu::WIDTH;
            let mut px0 = 0;
            while px0 < ppu::WIDTH {
                let h = self.height[hrow + px0];
                let mut px1 = px0 + 1;
                while px1 < ppu::WIDTH && self.height[hrow + px1] == h {
                    px1 += 1;
                }
                // Split borrows: take the top texture out while strips run.
                let tex = std::mem::take(&mut self.top_tex);
                self.strip(px0, px1, py, h, &tex[py * ppu::WIDTH..(py + 1) * ppu::WIDTH]);
                self.top_tex = tex;
                px0 = px1;
            }
        }
        self.render_walls(cap);

        self.render_sprites_entry(cap);
        if self.tilt > 0 {
            self.tilt_shift(self.tilt);
        }
        // Dialogue boxes and menus (BG0) composite flat in screen space on
        // top of the finished 3D scene — UI must never extrude.
        if ui_pixels > 0 {
            self.blit_2d(&cap.ui_frame, true);
        }
    }

    /// Walls at height discontinuities. Faces are merged over runs of equal
    /// (top, bottom) height so neighboring columns never rasterize seams,
    /// and textured from the block's own map pixels (rows behind the face
    /// for south walls, columns into the block for side walls), darkened.
    /// This is what keeps a 16-unit tree face looking like tree art instead
    /// of a single pixel row smeared into vertical stripes.
    fn render_walls(&mut self, cap: &Capture) {
        if std::env::var("GBA_NO_WALLS").is_ok() {
            return;
        }
        const W: usize = ppu::WIDTH;
        const H: usize = ppu::HEIGHT;
        // Take the heightfield out of self so the raster methods can borrow
        // self mutably while we read it.
        let height = std::mem::take(&mut self.height);
        let hgt = |x: usize, y: usize| height[y * W + x];
        // South-facing walls (toward the camera), merged along x.
        for py in 0..H {
            let mut px0 = 0;
            while px0 < W {
                let h = hgt(px0, py);
                // Below the screen's bottom row lies the diorama's cut
                // plane: draw the face so cut buildings show a cross
                // section instead of a floating slab.
                let hs = if py + 1 < H { hgt(px0, py + 1) } else { 0.0 };
                if hs >= h {
                    px0 += 1;
                    continue;
                }
                let mut px1 = px0 + 1;
                while px1 < W
                    && hgt(px1, py) == h
                    && (if py + 1 < H { hgt(px1, py + 1) } else { 0.0 }) == hs
                {
                    px1 += 1;
                }
                let (ax, _, az) = world(px0 as f32, py as f32 + 1.0, 0.0);
                let bx = ax + (px1 - px0) as f32;
                let q = [
                    project(ax, h, az),
                    project(bx, h, az),
                    project(bx, hs, az),
                    project(ax, hs, az),
                ];
                let (n, dh) = ((px1 - px0) as f32, h - hs);
                self.quad_uv(q, &mut |u: f32, v: f32| {
                    let col = px0 + ((u * n) as usize).min(px1 - px0 - 1);
                    // Fold the art upright: the drawing's bottom row lands
                    // at the wall's bottom, not mirrored.
                    let row = py.saturating_sub(((1.0 - v.clamp(0.0, 1.0)) * (dh - 1.0)) as usize);
                    shade(cap.bg_frame[row * W + col], 0.55)
                });
                px0 = px1;
            }
        }
        // West / east side walls, merged along y.
        for px in 0..W {
            for west in [true, false] {
                let mut py0 = 0;
                while py0 < H {
                    let h = hgt(px, py0);
                    let hn = match west {
                        true if px > 0 => hgt(px - 1, py0),
                        false if px + 1 < W => hgt(px + 1, py0),
                        _ => h,
                    };
                    if hn >= h {
                        py0 += 1;
                        continue;
                    }
                    let mut py1 = py0 + 1;
                    while py1 < H && hgt(px, py1) == h && {
                        let n2 = match west {
                            true if px > 0 => hgt(px - 1, py1),
                            false if px + 1 < W => hgt(px + 1, py1),
                            _ => h,
                        };
                        n2 == hn
                    } {
                        py1 += 1;
                    }
                    let plane = if west { px as f32 } else { px as f32 + 1.0 };
                    let (ax, _, az0) = world(plane, py0 as f32, 0.0);
                    let az1 = az0 - (py1 - py0) as f32;
                    let q = [
                        project(ax, h, az0),
                        project(ax, h, az1),
                        project(ax, hn, az1),
                        project(ax, hn, az0),
                    ];
                    let (n, dh) = ((py1 - py0) as f32, h - hn);
                    self.quad_uv(q, &mut |u: f32, v: f32| {
                        let row = py0 + ((u * n) as usize).min(py1 - py0 - 1);
                        let k = (v.max(0.0) * dh) as usize;
                        let col = if west { (px + k).min(W - 1) } else { px.saturating_sub(k) };
                        shade(cap.bg_frame[row * W + col], 0.42)
                    });
                    py0 = py1;
                }
            }
        }
        self.height = height;
    }

    fn render_sprites_entry(&mut self, cap: &Capture) {
        if std::env::var("GBA_SPR_DEBUG").is_ok() {
            eprintln!("sprite pixels: {}", cap.sprite_pixels.len());
        }
        self.render_sprites(cap);
    }

    /// Sprites: group captured sprite pixels into connected figures, then
    /// draw each as a vertical billboard of voxels standing at its feet row,
    /// with a soft contact shadow on the ground.
    fn render_sprites(&mut self, cap: &Capture) {
        const W: usize = ppu::WIDTH;
        // Grid of sprite pixels for clustering.
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
            let ground = self.height[(feet as usize - 1).min(ppu::HEIGHT - 1) * W
                + ((min_x + max_x) as usize / 2).min(W - 1)];

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

            // The figure itself: ONE flat alpha-cut quad wearing the sprite
            // frame, standing at its feet and leaning back by exactly the
            // camera pitch (feet pivot), so it reads face-on like the flat
            // game. A sprite is a drawing, not an object seen from one
            // side; no geometry is built from its pixels.
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

/// Sentinel a `tri_uv` sampler returns for transparent texels: the pixel is
/// skipped entirely (no color, no depth), cutting the silhouette out of the
/// quad.
const SKIP: u32 = 0xFFFF_FFFF;

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
                if o < 0.05 {
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
