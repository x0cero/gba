//! Experimental "voxel diorama" renderer (--3d flag, native frontend only).
//!
//! Rebuilds the captured PPU layers as a tilted-perspective diorama in the
//! style of the Gen1Recomp voxel mod: the ground is a real 3D heightfield
//! (scenery layers extrude upward as blocks with shaded side walls), and
//! sprites stand up as thin vertical voxel figures anchored by a soft
//! contact shadow. Everything is software-rasterized (z-buffered triangles),
//! no dependencies.
use gba::ppu::{self, Capture};

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 500;

/// Camera pitch down from horizontal, in degrees.
const PITCH_DEG: f32 = 52.0;
/// Camera distance from the diorama center (world units = GBA pixels).
const CAM_DIST: f32 = 340.0;
/// Focal length in screen pixels.
const FOCAL: f32 = 940.0;
const CX: f32 = WIDTH as f32 / 2.0;
const CY: f32 = 236.0;

/// Extrusion heights per scenery class, in world units.
const H_OVERLAY: f32 = 16.0; // BG1: house bodies, roofs, tree canopies
const H_PROP: f32 = 9.0; // BG3: trunks, signs, mailboxes, fences
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
    let (s, c) = PITCH_DEG.to_radians().sin_cos();
    let yv = wy * c + wz * s;
    let zv = CAM_DIST + wz * c - wy * s;
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
    /// Per-map-pixel terrain height, quantized per 8x8 tile.
    height: Vec<f32>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            buffer: vec![0; WIDTH * HEIGHT],
            zbuf: vec![f32::INFINITY; WIDTH * HEIGHT],
            height: vec![0.0; ppu::WIDTH * ppu::HEIGHT],
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

    /// Axis-aligned horizontal cell top: map rect (px..px+1, py..py+1) at
    /// height h.
    fn top_face(&mut self, px: f32, py: f32, h: f32, color: u32) {
        let (ax, ay, az) = world(px, py, h);
        let q = [
            project(ax, ay, az),
            project(ax + 1.0, ay, az),
            project(ax + 1.0, ay, az - 1.0),
            project(ax, ay, az - 1.0),
        ];
        self.quad(q, color, None);
    }

    /// Vertical wall along a cell edge from height h0 up to h1. `axis` 0 =
    /// wall runs along x (south/north face), 1 = along z (east/west face).
    fn wall(&mut self, px: f32, py: f32, axis: u8, h0: f32, h1: f32, color: u32) {
        let (ax, _, az) = world(px, py, 0.0);
        let (bx, bz) = if axis == 0 { (ax + 1.0, az) } else { (ax, az - 1.0) };
        let q = [
            project(ax, h1, az),
            project(bx, h1, bz),
            project(bx, h0, bz),
            project(ax, h0, az),
        ];
        self.quad(q, color, None);
    }

    /// Terrain height per map pixel: scenery layers extrude, quantized per
    /// 8x8 tile (majority class) so blocks read as deliberate architecture.
    fn build_heights(&mut self, cap: &Capture) {
        let class = |i: usize| match cap.bg_layer[i] {
            1 => H_OVERLAY,
            3 => H_PROP,
            _ => 0.0,
        };
        for ty in 0..ppu::HEIGHT / 8 {
            for tx in 0..ppu::WIDTH / 8 {
                let (mut n_over, mut n_prop) = (0, 0);
                for dy in 0..8 {
                    for dx in 0..8 {
                        match class((ty * 8 + dy) * ppu::WIDTH + tx * 8 + dx) {
                            h if h == H_OVERLAY => n_over += 1,
                            h if h == H_PROP => n_prop += 1,
                            _ => {}
                        }
                    }
                }
                let h = if n_over >= 24 {
                    H_OVERLAY
                } else if n_over + n_prop >= 24 {
                    H_PROP
                } else {
                    0.0
                };
                for dy in 0..8 {
                    for dx in 0..8 {
                        self.height[(ty * 8 + dy) * ppu::WIDTH + tx * 8 + dx] = h;
                    }
                }
            }
        }
    }

    /// Draw one diorama frame from the captured layers.
    pub fn render(&mut self, cap: &Capture) {
        // Background: subtle vertical gradient.
        for y in 0..HEIGHT {
            let t = y as f32 / HEIGHT as f32;
            let lerp = |a: u32, b: u32, s: u32| {
                let (a, b) = (a >> s & 0xFF, b >> s & 0xFF);
                ((a as f32 + (b as f32 - a as f32) * t) as u32) << s
            };
            let c = lerp(BACKGROUND_TOP, BACKGROUND_BOT, 16)
                | lerp(BACKGROUND_TOP, BACKGROUND_BOT, 8)
                | lerp(BACKGROUND_TOP, BACKGROUND_BOT, 0);
            self.buffer[y * WIDTH..(y + 1) * WIDTH].fill(c);
        }
        self.zbuf.fill(f32::INFINITY);
        if cap.bg_frame.len() < ppu::WIDTH * ppu::HEIGHT {
            return; // no capture yet (first frame)
        }
        self.build_heights(cap);

        // Terrain: top faces everywhere, walls at height discontinuities.
        let no_terrain = std::env::var("GBA_NO_TERRAIN").is_ok();
        for py in 0..ppu::HEIGHT {
            if no_terrain {
                break;
            }
            for px in 0..ppu::WIDTH {
                let i = py * ppu::WIDTH + px;
                let h = self.height[i];
                let color = cap.bg_frame[i];
                self.top_face(px as f32, py as f32, h, color);
                if h <= 0.0 {
                    continue;
                }
                let wall_c = shade(color, 0.55);
                let side_c = shade(color, 0.42);
                // South wall (faces the camera).
                let hs = if py + 1 < ppu::HEIGHT { self.height[i + ppu::WIDTH] } else { 0.0 };
                if hs < h {
                    self.wall(px as f32, py as f32 + 1.0, 0, hs, h, wall_c);
                }
                // West / east walls.
                let hw = if px > 0 { self.height[i - 1] } else { 0.0 };
                if hw < h {
                    self.wall(px as f32, py as f32, 1, hw, h, side_c);
                }
                let he = if px + 1 < ppu::WIDTH { self.height[i + 1] } else { 0.0 };
                if he < h {
                    self.wall(px as f32 + 1.0, py as f32, 1, he, h, side_c);
                }
            }
        }

        if std::env::var("GBA_SPR_DEBUG").is_ok() {
            let (mut minx, mut maxx, mut miny, mut maxy) = (255u8, 0u8, 255u8, 0u8);
            for &(x, y, _) in &cap.sprite_pixels {
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
            }
            eprintln!(
                "sprite pixels: {} bbox x {}-{} y {}-{}",
                cap.sprite_pixels.len(),
                minx,
                maxx,
                miny,
                maxy
            );
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
            if std::env::var("GBA_SPR_DEBUG").is_ok() {
                let top = pixels.iter().map(|i| i / W).min().unwrap();
                eprintln!(
                    "cluster: {} px, x {}-{}, y {}-{}",
                    pixels.len(),
                    min_x,
                    max_x,
                    top,
                    feet
                );
            }
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
            // Billboard rows are stretched by 1/cos(pitch) so the sprite art
            // projects 1:1 on screen instead of foreshortening into a slab.
            let vscale = 1.0 / PITCH_DEG.to_radians().cos();
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
