//! Solid scenery for the native FireRed renderer. Meshes stay in map space;
//! their dimensions never depend on camera position or the captured pixels.
use super::*;

pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GBA_3D_STYLE").as_deref() == Ok("modeled"))
}

type Point = [f32; 3];

fn lighting(p: [Point; 3]) -> f32 {
    let a = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
    let b = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
    let n = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(0.001);
    let sun = (-0.45 * n[0] + 0.8 * n[1] - 0.4 * n[2]) / length;
    0.66 + 0.40 * sun.max(0.0)
}

/// Closed surface of revolution. Profile endpoints have radius zero.
/// A fixed angular phase keeps the faceting identical while walking.
fn lathe(profile: &[(f32, f32)], sides: usize, stretch: f32) -> Vec<[Point; 3]> {
    let mut mesh = Vec::new();
    for ring in profile.windows(2) {
        for side in 0..sides {
            let point = |level: usize, i: usize| {
                let angle = std::f32::consts::TAU * i as f32 / sides as f32 + 0.17;
                [
                    ring[level].1 * angle.cos(),
                    ring[level].0,
                    ring[level].1 * angle.sin() * stretch,
                ]
            };
            let a = point(0, side);
            let b = point(0, side + 1);
            let c = point(1, side + 1);
            let d = point(1, side);
            if ring[0].1 > 0.0 {
                mesh.push([a, c, b]);
            }
            if ring[1].1 > 0.0 {
                mesh.push([a, d, c]);
            }
        }
    }
    mesh
}

fn tree_mesh() -> &'static [[Point; 3]] {
    static MESH: std::sync::OnceLock<Vec<[Point; 3]>> = std::sync::OnceLock::new();
    MESH.get_or_init(|| {
        lathe(
            &[
                (8.0, 0.0),
                (10.0, 6.0),
                (14.0, 12.5),
                (20.0, 13.5),
                (27.0, 10.0),
                (33.0, 5.5),
                (37.0, 0.0),
            ],
            10,
            0.88,
        )
    })
}

fn trunk_mesh() -> &'static [[Point; 3]] {
    static MESH: std::sync::OnceLock<Vec<[Point; 3]>> = std::sync::OnceLock::new();
    MESH.get_or_init(|| lathe(&[(0.0, 0.0), (0.0, 2.8), (12.0, 1.9), (12.0, 0.0)], 7, 1.0))
}

/// Average only the actual green leaves, excluding turf, outlines and sky.
fn leaf_color(g: &MapGrid, cx: i32, cy: i32, w: u8, h: u8) -> u32 {
    let (mut r, mut green, mut b, mut count) = (0u32, 0u32, 0u32, 0u32);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let slot = g.slot_at(cx + x, cy + y, 0);
            for py in 0..16 {
                for px in 0..16 {
                    let c = g.art.object_at(slot, px, py);
                    let (cr, cg, cb) = (c >> 16 & 255, c >> 8 & 255, c & 255);
                    if c != SKIP && cg > 48 && cg * 10 > cr * 12 && cg * 10 > cb * 13 {
                        r += cr;
                        green += cg;
                        b += cb;
                        count += 1;
                    }
                }
            }
        }
    }
    match (
        r.checked_div(count),
        green.checked_div(count),
        b.checked_div(count),
    ) {
        (Some(r), Some(green), Some(b)) => r << 16 | green << 8 | b,
        _ => g.art.flat[g.slot_at(cx, cy, 0)],
    }
}

fn roof_height(t: f32, eave: f32, rise: f32) -> f32 {
    eave + rise * (1.0 - (2.0 * t - 1.0).abs()).max(0.0)
}

impl Renderer {
    pub(super) fn render_tree_floor(&mut self, g: &MapGrid, cx: i32, cy: i32, unit: [u8; 4]) {
        let [w, h, dx, dy] = unit;
        let x = g.ox + cx as f32 * STEP;
        let z = g.oz - cy as f32 * STEP;
        let q = [
            [x, 0.0, z],
            [x + STEP, 0.0, z],
            [x + STEP, 0.0, z - STEP],
            [x, 0.0, z - STEP],
        ];
        self.quad_uv(q.map(|p| project(p[0], p[1], p[2])), &mut |u, v| {
            let tx = (dx as f32 + u - w as f32 * 0.5) / (w as f32 * 0.48);
            let ty = (dy as f32 + v - h as f32 * 0.5) / (h as f32 * 0.48);
            let shadow = (1.0 - tx * tx - ty * ty).clamp(0.0, 1.0);
            let c = g.art.comp_at(
                g.model_floor,
                (u * 16.0).clamp(0.0, 15.0) as usize,
                (v * 16.0).clamp(0.0, 15.0) as usize,
            );
            shade(c, 0.96 - shadow * 0.28)
        });
    }

    fn model_triangle(&mut self, p: [Point; 3], color: u32) {
        let c = shade(color, lighting(p));
        let q = p.map(|p| project(p[0], p[1], p[2]));
        self.tri_uv(q, [(0.0, 0.0); 3], &mut |_, _| c);
    }

    pub(super) fn render_tree_model(&mut self, g: &MapGrid, cx: i32, cy: i32, w: u8, h: u8) {
        let wx = g.ox + (cx as f32 + w as f32 * 0.5) * STEP;
        let wz = g.oz - (cy as f32 + h as f32 * 0.5) * STEP;
        let scale = (w.min(h) as f32 / 2.0).clamp(0.48, 1.3);
        let color = leaf_color(g, cx, cy, w, h);
        let fade = ((color >> 8 & 255) as f32 / 150.0).min(1.0);
        self.marking = self.tree_at.len() as u16;
        self.tree_at.push((g.gx0 + cx, g.gy0 + cy));
        for (mesh, material) in [(trunk_mesh(), shade(0x896044, fade)), (tree_mesh(), color)] {
            for triangle in mesh {
                let points = triangle.map(|v| [wx + v[0] * scale, v[1] * scale, wz + v[2] * scale]);
                self.model_triangle(points, material);
            }
        }
        self.marking = NO_TREE;
        if trace() {
            println!(
                "MODEL tree {} {} triangles {}",
                g.gx0 + cx,
                g.gy0 + cy,
                tree_mesh().len() + trunk_mesh().len()
            );
        }
    }

    /// Only outdoor multi-row structures receive roofs. Furniture and thin
    /// obstacles continue to use their existing geometry.
    pub(super) fn render_building_cell(&mut self, g: &MapGrid, cx: i32, cy: i32) -> bool {
        let Cell::Block { n, k, wall, .. } = g.at(cx, cy) else {
            return false;
        };
        if g.indoors || n < 3 || wall == 0 {
            return false;
        }
        let (n, k) = (n as i32, k as i32);
        let top = cy - k;
        let slot = g.slot_at(cx, top, 0);
        let (x0, x1) = (g.ox + cx as f32 * STEP, g.ox + (cx + 1) as f32 * STEP);
        let north = g.oz - top as f32 * STEP;
        let south = north - n as f32 * STEP;
        let eave = ((n - 1) as f32 * 10.0).clamp(20.0, 36.0);
        let rise = (n as f32 * 3.0).min(18.0);
        let same_run =
            |x| matches!(g.at(x,top), Cell::Block{n:nn,k:0,wall:ww,..} if nn as i32==n && ww>0);
        let (mut left, mut right) = (cx, cx);
        while same_run(left - 1) {
            left -= 1;
        }
        while same_run(right + 1) {
            right += 1;
        }
        let middle = (left + right) / 2;
        let roof_color = g.art.flat[g.slot_at(middle, top, slot)];
        let wall_slot = g.slot_at(middle, top + n - 2, slot);
        let wall_color = g.art.flat[wall_slot];
        // One planar roof strip per map row, split exactly at the ridge.
        // Adjacent columns share vertices, so no stair-step roof seams form.
        let t0 = k as f32 / n as f32;
        let t1 = (k + 1) as f32 / n as f32;
        let mut cuts = vec![t0];
        if t0 < 0.5 && t1 > 0.5 {
            cuts.push(0.5);
        }
        cuts.push(t1);
        for span in cuts.windows(2) {
            let (a, b) = (span[0], span[1]);
            let za = north - a * n as f32 * STEP;
            let zb = north - b * n as f32 * STEP;
            let ha = roof_height(a, eave, rise);
            let hb = roof_height(b, eave, rise);
            let p = [[x0, ha, za], [x1, ha, za], [x1, hb, zb], [x0, hb, zb]];
            let light = lighting([p[0], p[1], p[2]]);
            self.quad_uv(p.map(|p| project(p[0], p[1], p[2])), &mut |u, v| {
                let depth = (a + v * (b - a)) * n as f32 * STEP;
                let seam = depth.rem_euclid(8.0) < 0.8;
                let joint = (u * STEP
                    + if (depth / 8.0) as i32 % 2 == 0 {
                        0.0
                    } else {
                        8.0
                    })
                .rem_euclid(16.0)
                    < 0.6;
                shade(roof_color, light * if seam || joint { 0.82 } else { 1.0 })
            });
            for (edge, west, nx) in [(x0, true, cx - 1), (x1, false, cx + 1)] {
                if matches!(g.at(nx,cy),Cell::Block {n:nn,k:kk,wall:ww,..} if nn as i32==n && kk as i32==k && ww>0)
                {
                    continue;
                }
                let mut q = [
                    [edge, ha, za],
                    [edge, hb, zb],
                    [edge, 0.0, zb],
                    [edge, 0.0, za],
                ];
                if !west {
                    q.reverse();
                }
                self.model_triangle([q[0], q[1], q[2]], wall_color);
                self.model_triangle([q[0], q[2], q[3]], wall_color);
            }
        }
        if k == n - 1 {
            // Preserve the actual facade, including windows and animated doors.
            // The first structural row is roof; the remaining rows are walls.
            let q = [
                [x0, eave, south],
                [x1, eave, south],
                [x1, 0.0, south],
                [x0, 0.0, south],
            ];
            self.quad_uv(q.map(|p| project(p[0], p[1], p[2])), &mut |u, v| {
                let y = (v.clamp(0.0, 0.999) * (n - 1) as f32 * 16.0) as usize;
                let s = g.slot_at(cx, top + 1 + y as i32 / 16, slot);
                shade(
                    g.art
                        .comp_at(s, (u * 16.0).clamp(0.0, 15.0) as usize, y % 16),
                    0.94,
                )
            });
            // A shallow projecting eave gives the roof a visible thickness.
            let lip = [
                [x0, eave + 0.3, south + 0.1],
                [x1, eave + 0.3, south + 0.1],
                [x1, eave + 0.3, south - 2.0],
                [x0, eave + 0.3, south - 2.0],
            ];
            let c = shade(roof_color, 0.9);
            self.quad_uv(lip.map(|p| project(p[0], p[1], p[2])), &mut |_, _| c);
            let face = [
                [x0, eave + 0.3, south - 2.0],
                [x1, eave + 0.3, south - 2.0],
                [x1, eave - 1.0, south - 2.0],
                [x0, eave - 1.0, south - 2.0],
            ];
            let c = shade(roof_color, 0.63);
            self.quad_uv(face.map(|p| project(p[0], p[1], p[2])), &mut |_, _| c);
        }
        if k == 0 {
            let p = [
                [x1, eave, north],
                [x0, eave, north],
                [x0, 0.0, north],
                [x1, 0.0, north],
            ];
            self.model_triangle([p[0], p[1], p[2]], wall_color);
            self.model_triangle([p[0], p[2], p[3]], wall_color);
            if trace() {
                println!(
                    "MODEL building {} {} rows {n} eave {eave} ridge {}",
                    g.gx0 + cx,
                    g.gy0 + top,
                    eave + rise
                );
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sloped_texture_and_depth_use_perspective() {
        let mut renderer = Renderer::new();
        renderer.tri_uv(
            [(0.0, 0.0, 1.0), (100.0, 0.0, 1.0), (0.0, 100.0, 2.0)],
            [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)],
            &mut |u, v| ((u * 10000.0) as u32) << 16 | ((v * 10000.0) as u32),
        );
        let at = 25 * WIDTH + 25;
        let z = 1.0 / (0.49 + 0.255 + 0.255 / 2.0);
        assert!((renderer.zbuf[at] - z).abs() < 0.00001);
        let color = renderer.buffer[at];
        assert!(((color >> 16) as i32 - (0.255 * z * 10000.0) as i32).abs() <= 1);
        assert!(((color & 65535) as i32 - (0.255 / 2.0 * z * 10000.0) as i32).abs() <= 1);
    }

    #[test]
    fn crown_is_volumetric_and_faces_outward() {
        for mesh in [tree_mesh(), trunk_mesh()] {
            assert!(mesh.len() >= 28);
            for p in mesh {
                assert!(p.iter().flatten().all(|c| c.is_finite()));
                let a = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
                let b = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
                let nx = a[1] * b[2] - a[2] * b[1];
                let nz = a[0] * b[1] - a[1] * b[0];
                let cx = p.iter().map(|v| v[0]).sum::<f32>();
                let cz = p.iter().map(|v| v[2]).sum::<f32>();
                assert!(nx * cx + nz * cz >= -0.001, "inward face");
            }
        }
    }

    #[test]
    fn roof_has_a_shared_ridge_and_equal_eaves() {
        assert_eq!(roof_height(0.0, 30.0, 12.0), 30.0);
        assert_eq!(roof_height(1.0, 30.0, 12.0), 30.0);
        assert_eq!(roof_height(0.5, 30.0, 12.0), 42.0);
        assert_eq!(roof_height(0.25, 30.0, 12.0), roof_height(0.75, 30.0, 12.0));
    }
}
