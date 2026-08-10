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
use gba::ppu::{self, Capture};

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 500;

/// Camera pitch down from horizontal: 52 degrees.
const SIN_P: f32 = 0.788_010_7;
const COS_P: f32 = 0.615_661_5;
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

pub struct Renderer {
    pub buffer: Vec<u32>,
    zbuf: Vec<f32>,
    /// Per-map-pixel terrain height, from the metatile classifier.
    height: Vec<f32>,
    background: Vec<u32>,
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
            background,
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

    fn quad(&mut self, q: [(f32, f32, f32); 4], color: u32, dim: Option<f32>) {
        self.tri([q[0], q[1], q[2]], color, dim);
        self.tri([q[0], q[2], q[3]], color, dim);
    }

    /// Z-buffered textured triangle: uv interpolated barycentrically,
    /// color from `sample(u, v)`.
    fn tri_uv(
        &mut self,
        p: [(f32, f32, f32); 3],
        uv: [(f32, f32); 3],
        sample: &mut impl FnMut(f32, f32) -> u32,
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
                if z < self.zbuf[i] {
                    let u = w0 * uv[0].0 + w1 * uv[1].0 + w2 * uv[2].0;
                    let v = w0 * uv[0].1 + w1 * uv[1].1 + w2 * uv[2].1;
                    self.zbuf[i] = z;
                    self.buffer[i] = sample(u, v);
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

    /// Terrain height per map pixel. Classified per 16x16 metatile aligned
    /// to the BG scroll (FireRed's map blocks are 16x16): BG1 overlay tiles
    /// (houses, roofs, tree canopies) extrude tall, BG3 prop tiles split by
    /// color into low tall-grass (dominantly green) and fences/signs; brown
    /// prop tiles directly south of a tall tile merge into it (tree trunks
    /// under canopies), so trees read as single solid blocks. Heights ramp
    /// down near the frame edges so scenery scrolling in grows out of the
    /// ground instead of popping in as a bare side wall.
    fn build_heights(&mut self, cap: &Capture) {
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
            let edge_y = py.min(ppu::HEIGHT - 1 - py) as f32;
            for px in 0..ppu::WIDTH {
                let tx = ((px as i32 - ox) / 16) as usize;
                let edge = edge_y.min(px.min(ppu::WIDTH - 1 - px) as f32);
                self.height[py * ppu::WIDTH + px] =
                    tiles[ty * tw + tx].min((edge + 1.0) * EDGE_RAMP);
            }
        }
    }

    /// Draw one diorama frame from the captured layers.
    pub fn render(&mut self, cap: &Capture) {
        self.buffer.copy_from_slice(&self.background);
        self.zbuf.fill(f32::INFINITY);
        if cap.bg_frame.len() < ppu::WIDTH * ppu::HEIGHT {
            return; // no capture yet (first frame)
        }
        self.build_heights(cap);

        // Terrain: textured strips over constant-height runs, then walls at
        // height discontinuities.
        for py in 0..ppu::HEIGHT {
            let row = &cap.bg_frame[py * ppu::WIDTH..(py + 1) * ppu::WIDTH];
            let hrow = py * ppu::WIDTH;
            let mut px0 = 0;
            while px0 < ppu::WIDTH {
                let h = self.height[hrow + px0];
                let mut px1 = px0 + 1;
                while px1 < ppu::WIDTH && self.height[hrow + px1] == h {
                    px1 += 1;
                }
                // Split borrows: strip reads `row` (from cap), writes self.
                let row_vec: &[u32] = row;
                self.strip(px0, px1, py, h, row_vec);
                px0 = px1;
            }
        }
        self.render_walls(cap);

        self.render_sprites_entry(cap);
    }

    /// Walls at height discontinuities. Faces are merged over runs of equal
    /// (top, bottom) height so neighboring columns never rasterize seams,
    /// and textured from the block's own map pixels (rows behind the face
    /// for south walls, columns into the block for side walls), darkened.
    /// This is what keeps a 16-unit tree face looking like tree art instead
    /// of a single pixel row smeared into vertical stripes.
    fn render_walls(&mut self, cap: &Capture) {
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
                let hs = if py + 1 < H { hgt(px0, py + 1) } else { h };
                if !(h > 0.0 && hs < h) {
                    px0 += 1;
                    continue;
                }
                let mut px1 = px0 + 1;
                while px1 < W
                    && hgt(px1, py) == h
                    && (if py + 1 < H { hgt(px1, py + 1) } else { h }) == hs
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
                    let row = py.saturating_sub((v.max(0.0) * dh) as usize);
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
                    if !(h > 0.0 && hn < h) {
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

            // Voxel billboard: each sprite pixel is a 1x1 face standing
            // upright at the feet line; thin top/side faces give it depth.
            // Rows are stretched by 1/cos(pitch) so the sprite art projects
            // 1:1 on screen instead of foreshortening into a slab.
            let vscale = 1.0 / COS_P;
            let thick = 1.4;
            for &i in &pixels {
                let (x, y) = ((i % W) as f32, (i / W) as f32);
                let color = grid[i] & 0x00FF_FFFF;
                let h0 = ground + (feet - y - 1.0) * vscale;
                let h1 = h0 + vscale;
                let (wx, _, wz) = world(x, feet - 1.0, 0.0);
                // Front face.
                self.quad(
                    [
                        project(wx, h1, wz),
                        project(wx + 1.0, h1, wz),
                        project(wx + 1.0, h0, wz),
                        project(wx, h0, wz),
                    ],
                    color,
                    None,
                );
                // Top face where no sprite pixel sits above.
                if y as usize == 0 || grid[i - W] == 0 {
                    self.quad(
                        [
                            project(wx, h1, wz + thick),
                            project(wx + 1.0, h1, wz + thick),
                            project(wx + 1.0, h1, wz),
                            project(wx, h1, wz),
                        ],
                        shade(color, 1.18),
                        None,
                    );
                }
                // Side faces at the figure's silhouette edges.
                if x as usize == 0 || grid[i - 1] == 0 {
                    self.quad(
                        [
                            project(wx, h1, wz + thick),
                            project(wx, h1, wz),
                            project(wx, h0, wz),
                            project(wx, h0, wz + thick),
                        ],
                        shade(color, 0.6),
                        None,
                    );
                }
                if x as usize + 1 >= W || grid[i + 1] == 0 {
                    self.quad(
                        [
                            project(wx + 1.0, h1, wz),
                            project(wx + 1.0, h1, wz + thick),
                            project(wx + 1.0, h0, wz + thick),
                            project(wx + 1.0, h0, wz),
                        ],
                        shade(color, 0.6),
                        None,
                    );
                }
            }
        }
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
